//! Native System V AMD64 (`x86_64`) Non-LLVM SOTA Optimizing Emitter (`TierName::Sota`).
//!
//! Shares the exact same `blaze-lir` mid-end (`SROA -> aegraph -> reassociation -> strength
//! reduction -> LICM -> bounded x86-64 e-graph superoptimizer`), then lowers directly to
//! native x86-64 machine code with:
//!
//! - **Loop-Weighted, Coalescing Chordal-SSA Register Allocation** over System V AMD64 GPRs
//!   (`rdi, rsi, rdx, rcx, r8, r9, rax, rbx, r12..r15`; `r10`/`r11` scratch) and `xmm0..xmm13`
//! - **Frameless Leaf Functions**: zero `push rbp; mov rbp, rsp` when no calls/spills/slots/callee-saves
//! - **3-Operand `lea` Fusion**: `add(a, shl(b, 1/2/3))` -> `lea [a + b*scale]`, non-destructive
//!   `add(a, b)` -> `lea [a + b]`, `add(a, imm32)` -> `lea [a + imm32]`, `mul(x, 3/5/9)` -> `lea [x + x*{2,4,8}]`
//! - **SIB Scaled Addressing**: `load`/`store(add/ptr_add(base, shl(idx, log2(size))))` -> `[base + idx*size]`
//! - **3-Operand `imul r, r, imm32`**, native 32-bit zero-extending ALU ops, `xor r32, r32` zeroing
//! - **Fused `test`/`cmp` + `jcc`/`cmovcc`**, **Granlund–Montgomery magic-multiply `udiv`** (`mul r64` -> `rdx`),
//!   **bitops** (`bswap`/`popcnt`/`lzcnt`/`tzcnt`/`rol`/`ror`), **atomics** (`lock xadd`/`xchg`/`mfence`)
//! - **Release AOT Export Mode (`CallMode::DirectBl`)**: direct `call rel32` (`RelocKind::Pc32`) +
//!   **Tail-Call Optimization** (`jmp rel32` / `jmp r11`) + cold-block sinking + 16-byte loop alignment!

use crate::Allocator;
use blaze_code::record::{OWN_RELRO, Space, Symbol, Target};
use blaze_lir::{
    AtomicRmw, Block, Flags, FloatCond, IntCond, Op, Ordering, Ty, View,
    func::{Conv, ParamKind, RefId, RefKind, SigId},
};
use blaze_own::{Alloc, Vec};
use blaze_tier::{Emitted, Named, Names, Outside, Reloc, RelocKind, TierError, TierName};

use super::CallMode;
use super::enc_x64::{
    self as x64, Cc, R8, R9, R10, R11, R12, R13, R14, R15, RAX, RBP, RBX, RCX, RDI, RDX, RSI, RSP, XReg,
};
use super::regalloc::{self, is_fp_ty, is_vec128_ty, is_vec256_ty, is_vec512_ty, is_vec_mask_ty};
use super::release_opts::{MagicUdiv64, compute_block_order, compute_magic_udiv64, detect_tail_calls};

const TIER: TierName = TierName::Sota;

/// System V AMD64 integer argument registers (`rdi, rsi, rdx, rcx, r8, r9`).
const SYSV_ARG_GPR: [XReg; 6] = [RDI, RSI, RDX, RCX, R8, R9];
/// System V AMD64 FP argument registers (`xmm0..xmm7`).
const SYSV_ARG_FP: [XReg; 8] = [0, 1, 2, 3, 4, 5, 6, 7];

/// Caller-saved allocatable GPRs (`r10`/`r11` permanently reserved as scratch/cycle-breakers).
const CALLER_SAVED_X64: &[XReg] = &[R8, R9, RDI, RSI, RAX, RDX, RCX];
/// Callee-saved allocatable GPRs (`rbx, r12..r15`).
const CALLEE_SAVED_X64: &[XReg] = &[RBX, R12, R13, R14, R15];
/// Caller-saved allocatable XMMs (`xmm14`/`xmm15` permanently reserved as FP scratch).
const CALLER_SAVED_XMM: &[XReg] = &[8, 9, 10, 11, 12, 13, 0, 1, 2, 3, 4, 5, 6, 7];

const XMM14: XReg = 14;
const XMM15: XReg = 15;

#[inline]
fn to_idx<T>(n: T) -> usize
where
    usize: TryFrom<T>,
{
    usize::try_from(n).unwrap_or(usize::MAX)
}

#[inline]
fn u32_of<T>(n: T) -> u32
where
    u32: TryFrom<T>,
{
    u32::try_from(n).unwrap_or(u32::MAX)
}

#[inline]
fn i32_of<T>(n: T) -> i32
where
    i32: TryFrom<T>,
{
    i32::try_from(n).unwrap_or(i32::MAX)
}

#[inline]
fn i64_of<T>(n: T) -> i64
where
    i64: TryFrom<T>,
{
    i64::try_from(n).unwrap_or(i64::MAX)
}

#[inline]
fn u8_of<T>(n: T) -> u8
where
    u8: TryFrom<T>,
{
    u8::try_from(n).unwrap_or(u8::MAX)
}

#[inline]
fn trunc_u32(n: u64) -> u32 {
    let b = n.to_le_bytes();
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

#[inline]
fn rel32_delta(target: usize, disp_at: usize) -> i32 {
    let next_ip = disp_at.saturating_add(4);
    if target >= next_ip {
        i32_of(target - next_ip)
    } else {
        i32_of(next_ip - target).wrapping_neg()
    }
}

#[inline]
fn spill_off(spill_base: i32, s: u32) -> i32 {
    spill_base.wrapping_add(i32_of(s).wrapping_mul(8))
}

#[inline]
fn at_u32(s: &[u32], i: usize) -> u32 {
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
fn at_usize(s: &[usize], i: usize) -> usize {
    s.get(i).copied().unwrap_or(0)
}

#[inline]
fn at_i32(s: &[i32], i: usize) -> i32 {
    s.get(i).copied().unwrap_or(0)
}

#[inline]
fn at_opt<T: Copy>(s: &[Option<T>], i: usize) -> Option<T> {
    s.get(i).copied().flatten()
}

#[inline]
fn at_loc(s: &[X64Loc], i: usize) -> X64Loc {
    s.get(i).copied().unwrap_or(X64Loc::None)
}

#[inline]
fn at_fusion(s: &[X64Fusion], i: usize) -> X64Fusion {
    s.get(i).copied().unwrap_or(X64Fusion::None)
}

#[inline]
fn ty_at(s: &[u16], i: usize) -> Ty {
    Ty(s.get(i).copied().unwrap_or(0))
}

#[inline]
fn set_at<T>(s: &mut [T], i: usize, v: T) {
    if let Some(slot) = s.get_mut(i) {
        *slot = v;
    }
}

#[inline]
fn inc_u32(s: &mut [u32], i: usize) {
    if let Some(slot) = s.get_mut(i) {
        *slot += 1;
    }
}

#[inline]
fn inc_sat_u32(s: &mut [u32], i: usize) {
    if let Some(slot) = s.get_mut(i) {
        *slot = slot.saturating_add(1);
    }
}

#[inline]
fn patch_rel32(code: &mut [u8], disp_at: usize, target_off: usize) {
    let rel = rel32_delta(target_off, disp_at);
    if let Some(slot) = code.get_mut(disp_at..disp_at + 4) {
        slot.copy_from_slice(&rel.to_le_bytes());
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum VecW {
    W128,
    W256,
    W512,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum X64Loc {
    None,
    Gpr(XReg),
    Xmm(XReg),
    GprSpill(u32),
    XmmSpill(u32),
    VecSpill(u32),
    Vec256Spill(u32),
    Vec512Spill(u32),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum X64Fusion {
    None,
    /// `add(a, shl(b, k))` (`k in 1..=3`) -> `lea rd, [a + b*(1<<k)]`.
    LeaAddScale { a: u32, b: u32, scale_log2: u8, imm_inst: u32 },
    /// `load`/`store` on `slot_addr(s)` -> `[rsp + off]`.
    MemRspOff { off: i32 },
    /// `load`/`store` on `add`/`ptr_add(base, imm32)` -> `[base + off]`.
    MemBaseOff { base: u32, off: i32, imm_inst: u32 },
    /// `load`/`store` on `add`/`ptr_add(base, shl(idx, log2(size)))` -> `[base + idx*size]`.
    MemSib { base: u32, idx: u32, scale_log2: u8, shift_imm: u32 },
    /// `br(icmp(cond, a, b))` -> `cmp`/`test` + `jcc`.
    CmpBr { cond: IntCond, a: u32, b: u32, op_ty: Ty },
    /// `br(icmp(eq/ne, and(x, imm32), 0))` -> `test r, imm32` + `jz/jnz`.
    TestImmBr { val: u32, mask_imm: u32, zero_inst: u32, w64: bool, on_nonzero: bool },
    /// `select(icmp(cond, a, b), t, f)` -> `cmp` + `cmovcc`.
    CmpSelect { cond: IntCond, a: u32, b: u32, op_ty: Ty },
    /// Contractible `fadd(acc, fmul(a, b))` -> FMA3 `vfmadd231{sd,ss,pd,ps}` / `vfmadd213`!
    Fma { a: u32, b: u32, acc: u32 },
}

#[derive(Clone, Copy)]
struct ActiveX64 {
    end: usize,
    reg: XReg,
    inst: usize,
    weight: u32,
}

#[derive(Clone, Copy)]
struct Rel32Patch {
    disp_at: usize,
    target_block: u32,
}

#[inline]
fn cc_of(c: IntCond) -> Cc {
    match c {
        IntCond::Eq => Cc::E,
        IntCond::Ne => Cc::Ne,
        IntCond::Ult => Cc::B,
        IntCond::Ule => Cc::Be,
        IntCond::Ugt => Cc::A,
        IntCond::Uge => Cc::Ae,
        IntCond::Slt => Cc::L,
        IntCond::Sle => Cc::Le,
        IntCond::Sgt => Cc::G,
        IntCond::Sge => Cc::Ge,
    }
}

#[inline]
fn is_supported_x64_scalar(ty: Ty) -> bool {
    ty == Ty::NONE || ty.is_ptr() || is_fp_ty(ty).is_some() || (ty.is_scalar() && ty.is_int() && ty.bits(64) <= 64)
}

#[inline]
fn mem_bytes_of(ty: Ty) -> Option<(u8, bool)> {
    if let Some(double) = is_fp_ty(ty) {
        return Some((if double { 8 } else { 4 }, true));
    }
    if ty.is_ptr() {
        return Some((8, false));
    }
    if ty.is_scalar() && ty.is_int() {
        let b = match ty.bits(64) {
            8 => 1,
            16 => 2,
            32 => 4,
            64 => 8,
            _ => return None,
        };
        return Some((b, false));
    }
    None
}

#[inline]
fn extract_const_u64(view: &View<'_>, inst: usize) -> Option<u64> {
    let op = view.op(inst);
    let ty = ty_at(view.tys, inst);
    if !(ty.is_scalar() && ty.is_int()) {
        return None;
    }
    let bits = ty.bits(64);
    if bits > 64 {
        return None;
    }
    let mask = if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 };
    match op {
        Op::Iconst => {
            let w = at_u32(view.data, inst);
            let v = if bits <= 32 { u64::from(w) } else { i64::from(w.cast_signed()).cast_unsigned() };
            Some(v & mask)
        }
        Op::IconstWide => {
            let at = to_idx(at_u32(view.data, inst));
            let lo = u64::from(at_u32(view.extra, at));
            let hi = if ty.const_words() >= 2 { u64::from(at_u32(view.extra, at + 1)) } else { 0 };
            Some((lo | (hi << 32)) & mask)
        }
        _ => None,
    }
}

fn check_x64_sig(view: &View<'_>, sig_id: SigId, inst: Option<u32>) -> Result<(), TierError> {
    let Some(rec) = view.sigs.get(sig_id.index()) else {
        return Err(TierError::Refused { tier: TIER, inst, what: "missing signature" });
    };
    if Conv::from_byte(rec.conv) != Some(Conv::C) || (rec.bits & blaze_lir::func::SigRec::VARIADIC != 0) {
        return Err(TierError::Refused { tier: TIER, inst, what: "non-C/variadic x86-64 signature" });
    }
    let sig = view.sig(sig_id);
    let (mut gpr_n, mut fp_n) = (0usize, 0usize);
    for p in sig.params {
        if !matches!(ParamKind::from_byte(p.kind), Some(ParamKind::Value | ParamKind::Sret)) {
            return Err(TierError::Refused { tier: TIER, inst, what: "byval parameter on x86-64" });
        }
        let ty = Ty(p.ty);
        if !is_supported_x64_scalar(ty) || ty == Ty::NONE {
            return Err(TierError::Refused { tier: TIER, inst, what: "unsupported x86-64 param type" });
        }
        if is_fp_ty(ty).is_some() {
            fp_n += 1;
        } else {
            gpr_n += 1;
        }
    }
    if gpr_n > SYSV_ARG_GPR.len() || fp_n > SYSV_ARG_FP.len() {
        return Err(TierError::Refused { tier: TIER, inst, what: "stacked x86-64 params (>6 GPR or >8 XMM)" });
    }
    if sig.rets.len() > 2 {
        return Err(TierError::Refused { tier: TIER, inst, what: "x86-64 return > 2 values" });
    }
    for r in sig.rets {
        let ty = Ty(r.ty);
        if ParamKind::from_byte(r.kind) != Some(ParamKind::Value) || !is_supported_x64_scalar(ty) || ty == Ty::NONE {
            return Err(TierError::Refused { tier: TIER, inst, what: "unsupported x86-64 return type" });
        }
    }
    Ok(())
}

#[inline]
fn is_x64_vec_ty(ty: Ty, has_avx2: bool, has_avx512: bool) -> Option<(u32, bool, VecW)> {
    if let Some((vsize, vfp)) = is_vec128_ty(ty) {
        return Some((vsize, vfp, VecW::W128));
    }
    if has_avx2 {
        if let Some((vsize, vfp)) = is_vec256_ty(ty) {
            return Some((vsize, vfp, VecW::W256));
        }
    }
    if has_avx512 {
        if let Some((vsize, vfp)) = is_vec512_ty(ty) {
            return Some((vsize, vfp, VecW::W512));
        }
    }
    None
}

fn check_x64_subset(view: &View<'_>, has_fma: bool, has_avx2: bool, has_avx512: bool) -> Result<(), TierError> {
    if view.head.ptr_bits != 64 || view.blocks() == 0 {
        return Err(TierError::Refused { tier: TIER, inst: None, what: "non-64-bit or empty x86-64 func" });
    }
    for b in 0..view.blocks() {
        if view.block_kind(Block(u32_of(b))).is_landing() {
            return Err(TierError::Refused { tier: TIER, inst: None, what: "x86-64 unwind landing block" });
        }
    }
    check_x64_sig(view, SigId(view.head.sig), None)?;
    for inst in 0..view.len() {
        let ty = ty_at(view.tys, inst);
        let vec_ty = is_x64_vec_ty(ty, has_avx2, has_avx512);
        let mask_ty = is_vec_mask_ty(ty);
        if !is_supported_x64_scalar(ty) && vec_ty.is_none() && mask_ty.is_none() {
            return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "non-scalar x86-64 type" });
        }
        let op = view.op(inst);
        let flags = Flags(at_u8(view.flags, inst));
        let honoured = match op {
            Op::Load | Op::Store => Flags::VOLATILE.0,
            Op::AtomicLoad | Op::AtomicStore | Op::AtomicRmw | Op::Fence => 0xff,
            _ => 0,
        };
        if op.info().flags.meanings_only(flags).0 & !honoured != 0 {
            return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unhonoured x86-64 flag" });
        }
        let ok = matches!(
            op,
            Op::Param
                | Op::Iconst
                | Op::IconstWide
                | Op::Fconst
                | Op::FconstWide
                | Op::Null
                | Op::Uninit
                | Op::Freeze
                | Op::Assume
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
                | Op::Udiv
                | Op::Sdiv
                | Op::Urem
                | Op::Srem
                | Op::Umulhi
                | Op::Smulhi
                | Op::Umin
                | Op::Umax
                | Op::Smin
                | Op::Smax
                | Op::Clz
                | Op::Ctz
                | Op::Popcnt
                | Op::Bswap
                | Op::Icmp
                | Op::Fcmp
                | Op::Select
                | Op::Zext
                | Op::Sext
                | Op::Trunc
                | Op::Fadd
                | Op::Fsub
                | Op::Fmul
                | Op::Fdiv
                | Op::Fma
                | Op::Fmuladd
                | Op::Bitcast
                | Op::SlotAddr
                | Op::SlotLive
                | Op::SlotDead
                | Op::PtrAdd
                | Op::PtrAddr
                | Op::Expose
                | Op::FromExposed
                | Op::PtrWithAddr
                | Op::Load
                | Op::Store
                | Op::AtomicLoad
                | Op::AtomicStore
                | Op::AtomicRmw
                | Op::Fence
                | Op::DataAddr
                | Op::FuncAddr
                | Op::Jump
                | Op::Br
                | Op::Return
                | Op::Trap
                | Op::Unreachable
                | Op::Splat
                | Op::ReduceAdd
                | Op::ReduceAnd
                | Op::ReduceOr
                | Op::ReduceXor
                | Op::ReduceUmax
                | Op::ReduceFadd
                | Op::Call
                | Op::CallIndirect
        );
        if !ok {
            return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: op.name() });
        }
        if matches!(op, Op::Fma | Op::Fmuladd) && !has_fma {
            return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "x86-64 FMA requires Machine::FMA" });
        }
        if mask_ty.is_some() {
            let mok = if op == Op::Icmp {
                let at = to_idx(at_u32(view.data, inst));
                let a_ty = ty_at(view.tys, to_idx(at_u32(view.extra, at + 1)));
                is_x64_vec_ty(a_ty, has_avx2, has_avx512)
                    .is_some_and(|(vsize, vfp, vw)| !vfp && vw != VecW::W512 && (has_avx2 || vsize < 3))
            } else {
                false
            };
            if !mok {
                return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unsupported x86-64 vector mask op" });
            }
        }
        if let Some((vsize, vfp, vw)) = vec_ty {
            let vok = if vw == VecW::W512 {
                !vfp && vsize >= 2
                    && matches!(
                        op,
                        Op::Param | Op::Load | Op::Uninit | Op::Freeze | Op::Add | Op::Sub | Op::And | Op::Or | Op::Xor
                            | Op::Splat
                    )
                    && (op != Op::Splat || extract_const_u64(view, to_idx(at_u32(view.data, inst))) == Some(0))
            } else {
                match op {
                    Op::Param | Op::Load | Op::Splat | Op::Uninit | Op::Freeze => true,
                    Op::Add | Op::Sub | Op::And | Op::Or | Op::Xor => !vfp,
                    Op::Mul => !vfp && (vsize == 1 || (has_avx2 && vsize == 2)), // pmullw / vpmulld
                    Op::Fadd | Op::Fsub | Op::Fmul => vfp,
                    Op::Fma | Op::Fmuladd => vfp && has_fma,
                    Op::Select => {
                        let at = to_idx(at_u32(view.data, inst));
                        is_vec_mask_ty(ty_at(view.tys, to_idx(at_u32(view.extra, at)))).is_some()
                    }
                    _ => false,
                }
            };
            if !vok {
                return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unsupported x86-64 vector op" });
            }
        }
        if matches!(op, Op::ReduceAdd | Op::ReduceAnd | Op::ReduceOr | Op::ReduceXor | Op::ReduceUmax | Op::ReduceFadd) {
            let src_v = if op == Op::ReduceFadd {
                to_idx(at_u32(view.extra, to_idx(at_u32(view.data, inst)) + 1))
            } else {
                to_idx(at_u32(view.data, inst))
            };
            let Some((vsize, vfp, vw)) = is_x64_vec_ty(ty_at(view.tys, src_v), has_avx2, has_avx512) else {
                return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "non-vector x86-64 reduce" });
            };
            if (op == Op::ReduceFadd) != vfp || (op == Op::ReduceUmax && vsize != 0) || (vw == VecW::W512 && (vfp || vsize < 2)) {
                return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "mismatched SSE2 reduce kind" });
            }
        }
        if matches!(op, Op::AtomicRmw) {
            let at = to_idx(at_u32(view.data, inst));
            let kind = AtomicRmw::from_word(at_u32(view.extra, at));
            if ty.bits(64) != 64 || !matches!(kind, Some(AtomicRmw::Add | AtomicRmw::Sub | AtomicRmw::Xchg)) {
                return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unsupported x86-64 atomic_rmw" });
            }
        }
        if matches!(op, Op::Call | Op::CallIndirect) {
            let at = to_idx(at_u32(view.data, inst));
            let sig_id = SigId(at_u32(view.extra, at + 1));
            check_x64_sig(view, sig_id, Some(u32_of(inst)))?;
        }
    }
    Ok(())
}

fn resolve_x64_outside<'a>(
    names: &dyn Names,
    of: Outside,
    space: Space,
    allow_c_name: bool,
    out: &mut Emitted<'a>,
    inst: usize,
) -> Result<u32, TierError> {
    match names.name(of) {
        Some(Named::Symbol(symbol)) => {
            let idx = u32_of(out.targets.len());
            out.targets.push(Target { space, symbol });
            Ok(idx)
        }
        Some(Named::C(name)) => {
            if !allow_c_name {
                return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "C name for non-func/static" });
            }
            let sym = Symbol::of_name(name);
            let start = u32_of(out.text.len());
            out.text.extend_from_slice(name.as_bytes());
            out.names.push((sym, start, u32_of(name.len())));
            let idx = u32_of(out.targets.len());
            out.targets.push(Target { space: Space::Outside, symbol: sym });
            Ok(idx)
        }
        None => Err(TierError::NoName { tier: TIER, of }),
    }
}

/// Resolves simultaneous parallel GPR copies over registers & stack spill slots using `R10`/`R11`.
fn emit_parallel_x64_gpr<A: Allocator>(
    buf: &mut Vec<u8, A>,
    moves: &mut Vec<(X64Loc, X64Loc), A>,
    spill_base: i32,
) {
    let mut i = 0;
    while let Some(&(dst, src)) = moves.get(i) {
        if dst == src {
            moves.remove_swap(i);
        } else {
            i += 1;
        }
    }
    let emit_one = |buf: &mut Vec<u8, A>, dst: X64Loc, src: X64Loc| match (dst, src) {
        (X64Loc::Gpr(d), X64Loc::Gpr(s)) => {
            if d != s {
                x64::mov_rr(buf, true, d, s);
            }
        }
        (X64Loc::Gpr(d), X64Loc::GprSpill(s)) => {
            x64::load_disp(buf, 8, d, RSP, spill_off(spill_base, s));
        }
        (X64Loc::GprSpill(d), X64Loc::Gpr(s)) => {
            x64::store_disp(buf, 8, s, RSP, spill_off(spill_base, d));
        }
        (X64Loc::GprSpill(d), X64Loc::GprSpill(s)) => {
            x64::load_disp(buf, 8, R11, RSP, spill_off(spill_base, s));
            x64::store_disp(buf, 8, R11, RSP, spill_off(spill_base, d));
        }
        _ => {}
    };
    while !moves.is_empty() {
        let mut ready = None;
        for (idx, &(dst, _)) in moves.iter().enumerate() {
            if !moves.iter().any(|&(_, s)| s == dst) {
                ready = Some(idx);
                break;
            }
        }
        if let Some(idx) = ready {
            let (dst, src) = moves.remove_swap(idx);
            emit_one(buf, dst, src);
        } else {
            let (dst, src) = moves.remove_swap(0);
            match dst {
                X64Loc::Gpr(d) => x64::mov_rr(buf, true, R10, d),
                X64Loc::GprSpill(s) => x64::load_disp(buf, 8, R10, RSP, spill_off(spill_base, s)),
                _ => {}
            }
            for m in moves.iter_mut() {
                if m.1 == dst {
                    m.1 = X64Loc::Gpr(R10);
                }
            }
            emit_one(buf, dst, src);
        }
    }
}

/// Resolves simultaneous parallel XMM/YMM copies using `XMM14`/`XMM15`.
fn emit_parallel_x64_xmm<A: Allocator>(
    buf: &mut Vec<u8, A>,
    moves: &mut Vec<(X64Loc, X64Loc), A>,
    spill_base: i32,
    use_256: bool,
) {
    let mut i = 0;
    while let Some(&(dst, src)) = moves.get(i) {
        if dst == src {
            moves.remove_swap(i);
        } else {
            i += 1;
        }
    }
    let emit_rr = |buf: &mut Vec<u8, A>, d: XReg, s: XReg| {
        if use_256 {
            x64::vmovdqa_rr(buf, true, d, s);
        } else {
            x64::movdqa_rr(buf, d, s);
        }
    };
    let emit_one = |buf: &mut Vec<u8, A>, dst: X64Loc, src: X64Loc| match (dst, src) {
        (X64Loc::Xmm(d), X64Loc::Xmm(s)) => {
            if d != s {
                emit_rr(buf, d, s);
            }
        }
        (X64Loc::Xmm(d), X64Loc::XmmSpill(s)) => {
            x64::load_fp_disp(buf, true, d, RSP, spill_off(spill_base, s));
        }
        (X64Loc::XmmSpill(d), X64Loc::Xmm(s)) => {
            x64::store_fp_disp(buf, true, s, RSP, spill_off(spill_base, d));
        }
        (X64Loc::XmmSpill(d), X64Loc::XmmSpill(s)) => {
            x64::load_fp_disp(buf, true, XMM15, RSP, spill_off(spill_base, s));
            x64::store_fp_disp(buf, true, XMM15, RSP, spill_off(spill_base, d));
        }
        (X64Loc::Xmm(d), X64Loc::VecSpill(s)) => {
            x64::movdqu_load(buf, d, RSP, spill_off(spill_base, s));
        }
        (X64Loc::VecSpill(d), X64Loc::Xmm(s)) => {
            x64::movdqu_store(buf, s, RSP, spill_off(spill_base, d));
        }
        (X64Loc::VecSpill(d), X64Loc::VecSpill(s)) => {
            x64::movdqu_load(buf, XMM15, RSP, spill_off(spill_base, s));
            x64::movdqu_store(buf, XMM15, RSP, spill_off(spill_base, d));
        }
        (X64Loc::Xmm(d), X64Loc::Vec256Spill(s)) => {
            x64::vmovdqu_load(buf, true, d, RSP, spill_off(spill_base, s));
        }
        (X64Loc::Vec256Spill(d), X64Loc::Xmm(s)) => {
            x64::vmovdqu_store(buf, true, s, RSP, spill_off(spill_base, d));
        }
        (X64Loc::Vec256Spill(d), X64Loc::Vec256Spill(s)) => {
            x64::vmovdqu_load(buf, true, XMM15, RSP, spill_off(spill_base, s));
            x64::vmovdqu_store(buf, true, XMM15, RSP, spill_off(spill_base, d));
        }
        (X64Loc::Xmm(d), X64Loc::Vec512Spill(s)) => {
            x64::avx512_vmovdqu64_load(buf, d, RSP, spill_off(spill_base, s));
        }
        (X64Loc::Vec512Spill(d), X64Loc::Xmm(s)) => {
            x64::avx512_vmovdqu64_store(buf, s, RSP, spill_off(spill_base, d));
        }
        (X64Loc::Vec512Spill(d), X64Loc::Vec512Spill(s)) => {
            x64::avx512_vmovdqu64_load(buf, XMM15, RSP, spill_off(spill_base, s));
            x64::avx512_vmovdqu64_store(buf, XMM15, RSP, spill_off(spill_base, d));
        }
        _ => {}
    };
    while !moves.is_empty() {
        let mut ready = None;
        for (idx, &(dst, _)) in moves.iter().enumerate() {
            if !moves.iter().any(|&(_, s)| s == dst) {
                ready = Some(idx);
                break;
            }
        }
        if let Some(idx) = ready {
            let (dst, src) = moves.remove_swap(idx);
            emit_one(buf, dst, src);
        } else {
            let (dst, src) = moves.remove_swap(0);
            match dst {
                X64Loc::Xmm(d) => emit_rr(buf, XMM14, d),
                X64Loc::XmmSpill(s) => x64::load_fp_disp(buf, true, XMM14, RSP, spill_off(spill_base, s)),
                X64Loc::VecSpill(s) => x64::movdqu_load(buf, XMM14, RSP, spill_off(spill_base, s)),
                X64Loc::Vec256Spill(s) => x64::vmovdqu_load(buf, true, XMM14, RSP, spill_off(spill_base, s)),
                X64Loc::Vec512Spill(s) => x64::avx512_vmovdqu64_load(buf, XMM14, RSP, spill_off(spill_base, s)),
                _ => {}
            }
            for m in moves.iter_mut() {
                if m.1 == dst {
                    m.1 = X64Loc::Xmm(XMM14);
                }
            }
            emit_one(buf, dst, src);
        }
    }
}

/// Emits native System V AMD64 machine code for verified, optimized `view`.
pub(crate) fn emit_sota_x64<'a>(
    view: View<'_>,
    names: &dyn Names,
    call_mode: CallMode,
    features: u64,
    alloc: Alloc<'a>,
) -> Result<Emitted<'a>, TierError> {
    let has_bmi2 = features & blaze_tier::Machine::BMI2 != 0;
    let has_fma = features & blaze_tier::Machine::FMA != 0;
    let has_avx2 = features & blaze_tier::Machine::AVX2 != 0;
    let has_avx512 = features & blaze_tier::Machine::AVX512 != 0;
    check_x64_subset(&view, has_fma, has_avx2, has_avx512)?;

    let len = view.len();
    let has_ymm = (has_avx2 && (0..len).any(|i| is_vec256_ty(ty_at(view.tys, i)).is_some()))
        || (has_avx512 && (0..len).any(|i| is_vec512_ty(ty_at(view.tys, i)).is_some()));
    let blocks = view.blocks();
    let tail_calls = detect_tail_calls(&view, call_mode == CallMode::DirectBl, alloc);
    let block_order = compute_block_order(&view, alloc);

    // 0. Layout local stack slots at `[rsp + 0..spill_base]`.
    let mut slot_cursor = 0u64;
    let mut slot_at: Vec<i32, Alloc<'a>> = Vec::with_capacity_in(view.slots.len(), alloc);
    for slot in view.slots {
        let align = 1u64 << slot.align.min(12);
        slot_cursor = slot_cursor.next_multiple_of(align);
        let Ok(off) = i32::try_from(slot_cursor) else {
            return Err(TierError::Refused { tier: TIER, inst: None, what: "x86-64 frame overflow" });
        };
        slot_at.push(off);
        slot_cursor += u64::from(slot.size);
    }
    let Ok(spill_base) = i32::try_from(slot_cursor.next_multiple_of(8)) else {
        return Err(TierError::Refused { tier: TIER, inst: None, what: "x86-64 frame overflow" });
    };

    // 1. Precompute block ownership, use counts, constants, and loop depths.
    let mut inst_block: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    let mut use_count: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    let mut const_u64: Vec<Option<u64>, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    for i in 0..len {
        inst_block.push(0);
        use_count.push(0);
        const_u64.push(extract_const_u64(&view, i));
    }
    let mut block_depth: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
    let mut is_loop_header: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        block_depth.push(0);
        is_loop_header.push(false);
    }
    for b in 0..blocks {
        let b_u32 = u32_of(b);
        for inst in view.block(Block(b_u32)) {
            set_at(&mut inst_block, inst, b_u32);
            regalloc::for_each_use(&view, inst, |v| {
                let vi = to_idx(v);
                if vi < len {
                    inc_sat_u32(&mut use_count, vi);
                }
            });
        }
        let term = view.terminator(Block(b_u32));
        regalloc::for_each_edge(&view, term, |tb, _, _| {
            let tbi = to_idx(tb);
            if tbi <= b {
                set_at(&mut is_loop_header, tbi, true);
                for lb in tbi..=b {
                    inc_sat_u32(&mut block_depth, lb);
                }
            }
        });
    }

    // 2. ISLE-style x86-64 pattern fusion (`lea [a + b*scale]`, SIB `[base + idx*scale]`, `cmp+jcc`, `cmp+cmovcc`).
    let mut fusion: Vec<X64Fusion, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    let mut skip_emit: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    let mut slot_folded: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    for _ in 0..len {
        fusion.push(X64Fusion::None);
        skip_emit.push(false);
        slot_folded.push(0);
    }
    for inst in 0..len {
        let op = view.op(inst);
        let ty = ty_at(view.tys, inst);
        let bits = if ty == Ty::NONE
            || is_fp_ty(ty).is_some()
            || is_x64_vec_ty(ty, has_avx2, has_avx512).is_some()
            || is_vec_mask_ty(ty).is_some()
        {
            0
        } else {
            ty.bits(64)
        };
        let b = at_u32(&inst_block, inst);
        match op {
            Op::Add if bits == 64 || bits == 32 => {
                let at = to_idx(at_u32(view.data, inst));
                let (lhs, rhs) = (at_u32(view.extra, at), at_u32(view.extra, at + 1));
                let try_sh = |sh: u32, other: u32| -> Option<(u32, u32, u8, u32)> {
                    let si = to_idx(sh);
                    if at_u32(&use_count, si) == 1 && at_u32(&inst_block, si) == b && view.op(si) == Op::Shl && !at_bool(&skip_emit, si) {
                        let sat = to_idx(at_u32(view.data, si));
                        let sb = at_u32(view.extra, sat);
                        let sk = at_u32(view.extra, sat + 1);
                        if let Some(k) = at_opt(&const_u64, to_idx(sk)).filter(|&k| (1..=3).contains(&k)) {
                            return Some((other, sb, u8_of(k), sk));
                        }
                    }
                    None
                };
                if let Some((a, sb, sc, sk)) = try_sh(rhs, lhs) {
                    set_at(&mut fusion, inst, X64Fusion::LeaAddScale { a, b: sb, scale_log2: sc, imm_inst: sk });
                    set_at(&mut skip_emit, to_idx(rhs), true);
                } else if let Some((a, sb, sc, sk)) = try_sh(lhs, rhs) {
                    set_at(&mut fusion, inst, X64Fusion::LeaAddScale { a, b: sb, scale_log2: sc, imm_inst: sk });
                    set_at(&mut skip_emit, to_idx(lhs), true);
                }
            }
            Op::Load | Op::Store => {
                let at = to_idx(at_u32(view.data, inst));
                let (ptr, off, access_ty) = if op == Op::Load {
                    (at_u32(view.extra, at), at_u32(view.extra, at + 1).cast_signed(), ty)
                } else {
                    let v = at_u32(view.extra, at);
                    (at_u32(view.extra, at + 1), at_u32(view.extra, at + 2).cast_signed(), ty_at(view.tys, to_idx(v)))
                };
                let bytes = if let Some((_, _, vw)) = is_x64_vec_ty(access_ty, has_avx2, has_avx512) {
                    match vw {
                        VecW::W128 => 16u8,
                        VecW::W256 => 32u8,
                        VecW::W512 => 64u8,
                    }
                } else {
                    mem_bytes_of(access_ty)
                        .ok_or(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unsupported x86-64 mem type" })?
                        .0
                };
                let pi = to_idx(ptr);
                let pop = view.op(pi);
                if pop == Op::SlotAddr {
                    if let Some(total) = at_i32(&slot_at, to_idx(at_u32(view.data, pi))).checked_add(off) {
                        set_at(&mut fusion, inst, X64Fusion::MemRspOff { off: total });
                        inc_u32(&mut slot_folded, pi);
                        continue;
                    }
                }
                if at_u32(&use_count, pi) == 1 && at_u32(&inst_block, pi) == b && !at_bool(&skip_emit, pi) {
                    if pop == Op::PtrAdd || (pop == Op::Add && ty_at(view.tys, pi).bits(64) == 64) {
                        if let X64Fusion::LeaAddScale { a: base, b: idx, scale_log2, imm_inst } = at_fusion(&fusion, pi) {
                            if off == 0 && (1u8 << scale_log2) == bytes {
                                set_at(&mut fusion, inst, X64Fusion::MemSib { base, idx, scale_log2, shift_imm: imm_inst });
                                set_at(&mut skip_emit, pi, true);
                                continue;
                            }
                        } else if at_fusion(&fusion, pi) == X64Fusion::None {
                            let pat = to_idx(at_u32(view.data, pi));
                            let (l, r) = (at_u32(view.extra, pat), at_u32(view.extra, pat + 1));
                            if let Some(cv) = at_opt(&const_u64, to_idx(r)).and_then(|v| i32::try_from(v.cast_signed()).ok()) {
                                if let Some(total) = cv.checked_add(off) {
                                    set_at(&mut fusion, inst, X64Fusion::MemBaseOff { base: l, off: total, imm_inst: r });
                                    set_at(&mut skip_emit, pi, true);
                                    continue;
                                }
                            } else if off == 0 && pop == Op::PtrAdd {
                                let ri = to_idx(r);
                                let want_sc = u8_of(bytes.trailing_zeros());
                                if want_sc > 0 && at_u32(&use_count, ri) == 1 && at_u32(&inst_block, ri) == b && view.op(ri) == Op::Shl && !at_bool(&skip_emit, ri) {
                                    let sat = to_idx(at_u32(view.data, ri));
                                    let idx = at_u32(view.extra, sat);
                                    let sk = at_u32(view.extra, sat + 1);
                                    if at_opt(&const_u64, to_idx(sk)) == Some(u64::from(want_sc)) {
                                        set_at(&mut fusion, inst, X64Fusion::MemSib { base: l, idx, scale_log2: want_sc, shift_imm: sk });
                                        set_at(&mut skip_emit, pi, true);
                                        set_at(&mut skip_emit, ri, true);
                                        continue;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Op::Br => {
                let at = to_idx(at_u32(view.data, inst));
                let cv = to_idx(at_u32(view.extra, at));
                if at_u32(&use_count, cv) == 1 && at_u32(&inst_block, cv) == b && view.op(cv) == Op::Icmp && !at_bool(&skip_emit, cv) {
                    let cat = to_idx(at_u32(view.data, cv));
                    if let Some(cond) = IntCond::from_word(at_u32(view.extra, cat)) {
                        let (a, bv) = (at_u32(view.extra, cat + 1), at_u32(view.extra, cat + 2));
                        let op_ty = ty_at(view.tys, to_idx(a));
                        let bits = op_ty.bits(64);
                        // Fuse `br(icmp(eq/ne, and(x, imm32), 0))` into `test x, imm32; jz/jnz`!
                        let ai = to_idx(a);
                        if (bits == 64 || bits == 32)
                            && matches!(cond, IntCond::Eq | IntCond::Ne)
                            && at_opt(&const_u64, to_idx(bv)) == Some(0)
                            && at_u32(&use_count, ai) == 1
                            && at_u32(&inst_block, ai) == b
                            && view.op(ai) == Op::And
                            && !at_bool(&skip_emit, ai)
                        {
                            let aat = to_idx(at_u32(view.data, ai));
                            let (al, ar) = (at_u32(view.extra, aat), at_u32(view.extra, aat + 1));
                            let mask_pair = if at_opt(&const_u64, to_idx(ar)).is_some_and(|v| i32::try_from(v.cast_signed()).is_ok()) {
                                Some((al, ar))
                            } else if at_opt(&const_u64, to_idx(al)).is_some_and(|v| i32::try_from(v.cast_signed()).is_ok()) {
                                Some((ar, al))
                            } else {
                                None
                            };
                            if let Some((val, mask_imm)) = mask_pair {
                                set_at(
                                    &mut fusion,
                                    inst,
                                    X64Fusion::TestImmBr {
                                        val,
                                        mask_imm,
                                        zero_inst: bv,
                                        w64: bits == 64,
                                        on_nonzero: cond == IntCond::Ne,
                                    },
                                );
                                set_at(&mut skip_emit, cv, true);
                                set_at(&mut skip_emit, ai, true);
                                continue;
                            }
                        }
                        set_at(&mut fusion, inst, X64Fusion::CmpBr { cond, a, b: bv, op_ty });
                        set_at(&mut skip_emit, cv, true);
                    }
                }
            }
            Op::Fadd if has_fma => {
                let fl = Flags(at_u8(view.flags, inst));
                let is_fp_or_vfp = is_fp_ty(ty).is_some() || is_x64_vec_ty(ty, has_avx2, has_avx512).is_some_and(|(_, vfp, _)| vfp);
                if is_fp_or_vfp && (fl.0 & (Flags::CONTRACT.0 | Flags::REASSOC.0) != 0) {
                    let at = to_idx(at_u32(view.data, inst));
                    let (lhs, rhs) = (at_u32(view.extra, at), at_u32(view.extra, at + 1));
                    let try_mul = |m: u32, acc: u32| -> Option<(u32, u32, u32)> {
                        let mi = to_idx(m);
                        if at_u32(&use_count, mi) == 1 && at_u32(&inst_block, mi) == b && view.op(mi) == Op::Fmul && !at_bool(&skip_emit, mi) {
                            let mat = to_idx(at_u32(view.data, mi));
                            return Some((at_u32(view.extra, mat), at_u32(view.extra, mat + 1), acc));
                        }
                        None
                    };
                    if let Some((a, mb, acc)) = try_mul(rhs, lhs) {
                        set_at(&mut fusion, inst, X64Fusion::Fma { a, b: mb, acc });
                        set_at(&mut skip_emit, to_idx(rhs), true);
                    } else if let Some((a, mb, acc)) = try_mul(lhs, rhs) {
                        set_at(&mut fusion, inst, X64Fusion::Fma { a, b: mb, acc });
                        set_at(&mut skip_emit, to_idx(lhs), true);
                    }
                }
            }
            Op::Select if is_x64_vec_ty(ty, has_avx2, has_avx512).is_none() => {
                let at = to_idx(at_u32(view.data, inst));
                let cv = to_idx(at_u32(view.extra, at));
                if is_fp_ty(ty).is_none() && at_u32(&use_count, cv) == 1 && at_u32(&inst_block, cv) == b && view.op(cv) == Op::Icmp && !at_bool(&skip_emit, cv) {
                    let cat = to_idx(at_u32(view.data, cv));
                    if let Some(cond) = IntCond::from_word(at_u32(view.extra, cat)) {
                        let (a, bv) = (at_u32(view.extra, cat + 1), at_u32(view.extra, cat + 2));
                        set_at(&mut fusion, inst, X64Fusion::CmpSelect { cond, a, b: bv, op_ty: ty_at(view.tys, to_idx(a)) });
                        set_at(&mut skip_emit, cv, true);
                    }
                }
            }
            _ => {}
        }
    }
    for inst in 0..len {
        if view.op(inst) == Op::SlotAddr && at_u32(&use_count, inst) > 0 && at_u32(&slot_folded, inst) == at_u32(&use_count, inst) {
            set_at(&mut skip_emit, inst, true);
        }
    }

    // Fold constants consumed exclusively in immediate positions.
    let mut imm_uses: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    for _ in 0..len {
        imm_uses.push(0);
    }
    let fits_i32 = |v: u64| i32::try_from(v.cast_signed()).is_ok();
    for inst in 0..len {
        if at_bool(&skip_emit, inst) {
            continue;
        }
        match at_fusion(&fusion, inst) {
            X64Fusion::LeaAddScale { imm_inst, .. } | X64Fusion::MemBaseOff { imm_inst, .. } => {
                inc_u32(&mut imm_uses, to_idx(imm_inst));
            }
            X64Fusion::MemSib { shift_imm, .. } => {
                inc_u32(&mut imm_uses, to_idx(shift_imm));
            }
            X64Fusion::TestImmBr { mask_imm, zero_inst, .. } => {
                inc_u32(&mut imm_uses, to_idx(mask_imm));
                inc_u32(&mut imm_uses, to_idx(zero_inst));
            }
            X64Fusion::CmpBr { b: bv, op_ty, .. } | X64Fusion::CmpSelect { b: bv, op_ty, .. } => {
                if (op_ty.bits(64) == 64 || op_ty.bits(64) == 32) && at_opt(&const_u64, to_idx(bv)).is_some_and(fits_i32) {
                    inc_u32(&mut imm_uses, to_idx(bv));
                }
            }
            X64Fusion::MemRspOff { .. } | X64Fusion::Fma { .. } => {}
            X64Fusion::None => {
                let op = view.op(inst);
                let ty = ty_at(view.tys, inst);
                let bits = if ty == Ty::NONE || is_fp_ty(ty).is_some() { 0 } else { ty.bits(64) };
                match op {
                    Op::Add | Op::And | Op::Or | Op::Xor | Op::Mul => {
                        let at = to_idx(at_u32(view.data, inst));
                        let (l, r) = (to_idx(at_u32(view.extra, at)), to_idx(at_u32(view.extra, at + 1)));
                        if at_opt(&const_u64, r).is_some_and(fits_i32) {
                            inc_u32(&mut imm_uses, r);
                        } else if at_opt(&const_u64, l).is_some_and(fits_i32) {
                            inc_u32(&mut imm_uses, l);
                        }
                    }
                    Op::Sub | Op::PtrAdd => {
                        let at = to_idx(at_u32(view.data, inst));
                        let r = to_idx(at_u32(view.extra, at + 1));
                        if at_opt(&const_u64, r).is_some_and(fits_i32) {
                            inc_u32(&mut imm_uses, r);
                        }
                    }
                    Op::Shl | Op::Lshr | Op::Ashr | Op::Rotl | Op::Rotr if bits == 64 || bits == 32 => {
                        let at = to_idx(at_u32(view.data, inst));
                        let r = to_idx(at_u32(view.extra, at + 1));
                        if at_opt(&const_u64, r).is_some() {
                            inc_u32(&mut imm_uses, r);
                        }
                    }
                    Op::Udiv if bits == 64 => {
                        let at = to_idx(at_u32(view.data, inst));
                        let r = to_idx(at_u32(view.extra, at + 1));
                        if at_opt(&const_u64, r).and_then(compute_magic_udiv64).is_some() {
                            inc_u32(&mut imm_uses, r);
                        }
                    }
                    Op::Icmp => {
                        let at = to_idx(at_u32(view.data, inst));
                        let (a, bv) = (to_idx(at_u32(view.extra, at + 1)), to_idx(at_u32(view.extra, at + 2)));
                        let ab = ty_at(view.tys, a).bits(64);
                        if (ab == 64 || ab == 32) && at_opt(&const_u64, bv).is_some_and(fits_i32) {
                            inc_u32(&mut imm_uses, bv);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    for inst in 0..len {
        if at_opt(&const_u64, inst).is_some() && at_u32(&use_count, inst) > 0 && at_u32(&imm_uses, inst) == at_u32(&use_count, inst) {
            set_at(&mut skip_emit, inst, true);
        }
    }

    // 3. Loop-Weighted, Coalescing Chordal-SSA Register Allocation for System V AMD64!
    let mut last_use: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    let mut has_any_use: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    let mut use_weight: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    let mut call_pos: Vec<usize, Alloc<'a>> = Vec::new_in(alloc);
    for i in 0..len {
        last_use.push(i);
        has_any_use.push(false);
        use_weight.push(0);
    }
    for b in 0..blocks {
        let bw = 1u32 << (at_u32(&block_depth, b).min(4) * 3);
        for inst in view.block(Block(u32_of(b))) {
            let op = view.op(inst);
            if matches!(op, Op::Call | Op::CallIndirect) && !at_bool(&tail_calls, inst) {
                call_pos.push(inst);
            }
            if let Some(slot) = use_weight.get_mut(inst) {
                *slot = slot.saturating_add(bw);
            }
            regalloc::for_each_use(&view, inst, |v| {
                let vi = to_idx(v);
                if vi < len {
                    set_at(&mut has_any_use, vi, true);
                    if let Some(slot) = use_weight.get_mut(vi) {
                        *slot = slot.saturating_add(bw);
                    }
                    if inst > at_usize(&last_use, vi) {
                        set_at(&mut last_use, vi, inst);
                    }
                }
            });
        }
    }
    for inst in (0..len).rev() {
        if at_bool(&skip_emit, inst) {
            let end = at_usize(&last_use, inst);
            regalloc::for_each_use(&view, inst, |v| {
                let vi = to_idx(v);
                if vi < len && end > at_usize(&last_use, vi) {
                    set_at(&mut last_use, vi, end);
                }
            });
        }
    }
    loop {
        let mut changed = false;
        for b in 0..blocks {
            let term = view.terminator(Block(u32_of(b)));
            regalloc::for_each_edge(&view, term, |tb, _, _| {
                let tstart = view.block(Block(tb)).start;
                if tstart <= term {
                    for v in 0..tstart {
                        let lu = at_usize(&last_use, v);
                        if at_bool(&has_any_use, v) && lu >= tstart && lu < term {
                            set_at(&mut last_use, v, term);
                            changed = true;
                        }
                    }
                }
            });
        }
        if !changed {
            break;
        }
        for inst in (0..len).rev() {
            if at_bool(&skip_emit, inst) {
                let end = at_usize(&last_use, inst);
                regalloc::for_each_use(&view, inst, |v| {
                    let vi = to_idx(v);
                    if vi < len && end > at_usize(&last_use, vi) {
                        set_at(&mut last_use, vi, end);
                    }
                });
            }
        }
    }

    let mut loc_of: Vec<X64Loc, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    let mut hint: Vec<u8, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    let mut phi_partner: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    for _ in 0..len {
        loc_of.push(X64Loc::None);
        hint.push(u8::MAX);
        phi_partner.push(u32::MAX);
    }

    let entry_params = view.params(Block(0));
    let (mut g_arg, mut f_arg) = (0usize, 0usize);
    for p in entry_params.start..entry_params.end {
        if is_fp_ty(ty_at(view.tys, p)).is_some() {
            if let Some(&reg) = SYSV_ARG_FP.get(f_arg) {
                set_at(&mut hint, p, reg);
                f_arg += 1;
            }
        } else if let Some(&reg) = SYSV_ARG_GPR.get(g_arg) {
            set_at(&mut hint, p, reg);
            g_arg += 1;
        }
    }
    for b in 0..blocks {
        let term = view.terminator(Block(u32_of(b)));
        if view.op(term) == Op::Return {
            let at = to_idx(at_u32(view.data, term));
            let retc = to_idx(at_u32(view.extra, at));
            if retc >= 1 {
                let rv0 = to_idx(at_u32(view.extra, at + 1));
                if rv0 < len && at_u8(&hint, rv0) == u8::MAX {
                    set_at(&mut hint, rv0, if is_fp_ty(ty_at(view.tys, rv0)).is_some() { 0 } else { RAX });
                }
            }
            if retc == 2 {
                let rv1 = to_idx(at_u32(view.extra, at + 2));
                if rv1 < len && at_u8(&hint, rv1) == u8::MAX {
                    set_at(&mut hint, rv1, if is_fp_ty(ty_at(view.tys, rv1)).is_some() { 1 } else { RDX });
                }
            }
        } else {
            regalloc::for_each_edge(&view, term, |tb, astart, alen| {
                let tp = view.params(Block(tb));
                for k in 0..alen.min(tp.len()) {
                    let p = tp.start + k;
                    let arg = to_idx(at_u32(view.extra, astart + k));
                    if arg < len && arg > p && at_u32(&phi_partner, arg) == u32::MAX {
                        set_at(&mut phi_partner, arg, u32_of(p));
                    }
                }
            });
        }
    }

    let mut active_gpr: Vec<ActiveX64, Alloc<'a>> = Vec::new_in(alloc);
    let mut active_xmm: Vec<ActiveX64, Alloc<'a>> = Vec::new_in(alloc);
    let mut gpr_used = [false; 16];
    let mut xmm_used = [false; 16];
    let mut used_callee = [false; 16];
    let mut spill_busy: Vec<usize, Alloc<'a>> = Vec::new_in(alloc);
    let mut num_spills = 0u32;

    let alloc_spill = |busy: &mut Vec<usize, Alloc<'a>>, cnt: &mut u32, start: usize, end: usize| -> u32 {
        for (idx, until) in busy.iter_mut().enumerate() {
            if *until < start {
                *until = end;
                return u32_of(idx);
            }
        }
        let s = *cnt;
        *cnt += 1;
        busy.push(end);
        s
    };

    for inst in 0..len {
        let mut w = 0;
        while let Some(&a) = active_gpr.get(w) {
            if a.end < inst {
                set_at(&mut gpr_used, to_idx(a.reg), false);
                active_gpr.remove_swap(w);
            } else {
                w += 1;
            }
        }
        let mut w = 0;
        while let Some(&a) = active_xmm.get(w) {
            if a.end < inst {
                set_at(&mut xmm_used, to_idx(a.reg), false);
                active_xmm.remove_swap(w);
            } else {
                w += 1;
            }
        }
        let inst_ty = ty_at(view.tys, inst);
        if at_bool(&skip_emit, inst) || inst_ty == Ty::NONE || !at_bool(&has_any_use, inst) {
            continue;
        }
        let pp = at_u32(&phi_partner, inst);
        if at_u8(&hint, inst) == u8::MAX && pp != u32::MAX {
            match at_loc(&loc_of, to_idx(pp)) {
                X64Loc::Gpr(r) | X64Loc::Xmm(r) => set_at(&mut hint, inst, r),
                _ => {}
            }
        }
        let end = at_usize(&last_use, inst);
        let crosses_call = call_pos.iter().any(|&c| c > inst && c < end);
        let weight = at_u32(&use_weight, inst);

        let vinfo = is_x64_vec_ty(inst_ty, has_avx2, has_avx512);
        let is_mask = is_vec_mask_ty(inst_ty).is_some();
        let is_vec = vinfo.is_some() || is_mask;
        let vec_w = match vinfo {
            Some((_, _, vw)) => vw,
            None if is_mask && has_ymm && {
                let at = to_idx(at_u32(view.data, inst));
                view.op(inst) == Op::Icmp && is_vec256_ty(ty_at(view.tys, to_idx(at_u32(view.extra, at + 1)))).is_some()
            } => VecW::W256,
            None => VecW::W128,
        };
        if is_fp_ty(inst_ty).is_some() || is_vec {
            let alloc_vec_spill = |busy: &mut Vec<usize, Alloc<'a>>, cnt: &mut u32, start: usize, end: usize, slots: usize| -> u32 {
                if busy.len() >= slots {
                    for idx in 0..=(busy.len() - slots) {
                        if (0..slots).all(|k| at_usize(busy, idx + k) < start) {
                            for k in 0..slots {
                                set_at(busy, idx + k, end);
                            }
                            return u32_of(idx);
                        }
                    }
                }
                let s = *cnt;
                *cnt += u32_of(slots);
                for _ in 0..slots {
                    busy.push(end);
                }
                s
            };
            let spill_loc = |busy: &mut Vec<usize, Alloc<'a>>, cnt: &mut u32| -> X64Loc {
                if is_vec {
                    match vec_w {
                        VecW::W512 => X64Loc::Vec512Spill(alloc_vec_spill(busy, cnt, inst, end, 8)),
                        VecW::W256 => X64Loc::Vec256Spill(alloc_vec_spill(busy, cnt, inst, end, 4)),
                        VecW::W128 => X64Loc::VecSpill(alloc_vec_spill(busy, cnt, inst, end, 2)),
                    }
                } else {
                    X64Loc::XmmSpill(alloc_spill(busy, cnt, inst, end))
                }
            };
            if crosses_call {
                // SysV ABI has no callee-saved XMM/YMM/ZMM registers: spill cleanly across calls.
                set_at(&mut loc_of, inst, spill_loc(&mut spill_busy, &mut num_spills));
            } else {
                let h = at_u8(&hint, inst);
                let chosen = if h != u8::MAX && !at_bool(&xmm_used, to_idx(h)) && h < 14 {
                    Some(h)
                } else {
                    CALLER_SAVED_XMM.iter().copied().find(|&r| !at_bool(&xmm_used, to_idx(r)))
                };
                if let Some(r) = chosen {
                    set_at(&mut loc_of, inst, X64Loc::Xmm(r));
                    set_at(&mut xmm_used, to_idx(r), true);
                    active_xmm.push(ActiveX64 { end, reg: r, inst, weight });
                } else {
                    set_at(&mut loc_of, inst, spill_loc(&mut spill_busy, &mut num_spills));
                }
            }
        } else {
            let is_cs = |r: XReg| matches!(r, RBX | R12 | R13 | R14 | R15);
            let h = at_u8(&hint, inst);
            let chosen = if crosses_call {
                if h != u8::MAX && is_cs(h) && !at_bool(&gpr_used, to_idx(h)) {
                    Some(h)
                } else {
                    CALLEE_SAVED_X64.iter().copied().find(|&r| !at_bool(&gpr_used, to_idx(r)))
                }
            } else if h != u8::MAX && !at_bool(&gpr_used, to_idx(h)) && h != RSP && h != RBP && h != R10 && h != R11 {
                Some(h)
            } else {
                CALLER_SAVED_X64
                    .iter()
                    .copied()
                    .chain(CALLEE_SAVED_X64.iter().copied())
                    .find(|&r| !at_bool(&gpr_used, to_idx(r)))
            };
            if let Some(r) = chosen {
                set_at(&mut loc_of, inst, X64Loc::Gpr(r));
                set_at(&mut gpr_used, to_idx(r), true);
                if is_cs(r) {
                    set_at(&mut used_callee, to_idx(r), true);
                }
                active_gpr.push(ActiveX64 { end, reg: r, inst, weight });
            } else {
                // Loop-weighted Belady eviction: evict a strictly colder active register!
                let mut victim_best: Option<(usize, u32, usize)> = None;
                for (idx, a) in active_gpr.iter().enumerate() {
                    if a.weight >= weight || (crosses_call && !is_cs(a.reg)) {
                        continue;
                    }
                    let better = match victim_best {
                        None => true,
                        Some((_, bw, bend)) => a.weight < bw || (a.weight == bw && a.end > bend),
                    };
                    if better {
                        victim_best = Some((idx, a.weight, a.end));
                    }
                }
                if let Some((vidx, _, _)) = victim_best {
                    let victim = active_gpr.remove_swap(vidx);
                    let s = alloc_spill(&mut spill_busy, &mut num_spills, victim.inst, victim.end);
                    set_at(&mut loc_of, victim.inst, X64Loc::GprSpill(s));
                    set_at(&mut loc_of, inst, X64Loc::Gpr(victim.reg));
                    if is_cs(victim.reg) {
                        set_at(&mut used_callee, to_idx(victim.reg), true);
                    }
                    active_gpr.push(ActiveX64 { end, reg: victim.reg, inst, weight });
                } else {
                    let s = alloc_spill(&mut spill_busy, &mut num_spills, inst, end);
                    set_at(&mut loc_of, inst, X64Loc::GprSpill(s));
                }
            }
        }
    }

    // 4. Emit System V AMD64 machine bytes!
    let mut code: Vec<u8, Alloc<'a>> = Vec::with_capacity_in(len * 6 + 32, alloc);
    let mut patches: Vec<Rel32Patch, Alloc<'a>> = Vec::new_in(alloc);
    let mut block_byte_off: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        block_byte_off.push(0);
    }
    let mut out = Emitted::new_in(alloc);

    let mut saved_cs: Vec<XReg, Alloc<'a>> = Vec::with_capacity_in(5, alloc);
    for &r in CALLEE_SAVED_X64 {
        if at_bool(&used_callee, to_idx(r)) {
            saved_cs.push(r);
        }
    }
    let raw_frame = u64::try_from(spill_base).unwrap_or(0) + u64::from(num_spills) * 8;
    let has_calls = !call_pos.is_empty();
    let needs_frame = has_calls || !saved_cs.is_empty() || raw_frame > 0;
    // SysV ABI: on entry `rsp % 16 == 8`; `push rbp` makes `rsp % 16 == 0`; ensure total pushes + frame_bytes % 16 == 0!
    let cs_odd = saved_cs.len() % 2 == 1;
    let frame_bytes: i32 = if raw_frame == 0 && !cs_odd {
        0
    } else {
        let mut f = raw_frame.next_multiple_of(16);
        if cs_odd {
            f = (raw_frame + 8).next_multiple_of(16).saturating_sub(8);
            if f < raw_frame {
                f += 16;
            }
        }
        i32::try_from(f).map_err(|_| TierError::Refused { tier: TIER, inst: None, what: "x86-64 frame overflow" })?
    };

    if needs_frame {
        x64::push_r64(&mut code, RBP);
        x64::mov_rr(&mut code, true, RBP, RSP);
        for &r in saved_cs.iter() {
            x64::push_r64(&mut code, r);
        }
        if frame_bytes > 0 {
            x64::alu_ri(&mut code, true, 5, RSP, frame_bytes); // sub rsp, frame_bytes
        }
    }

    let emit_epilogue = |buf: &mut Vec<u8, Alloc<'a>>| {
        if has_ymm {
            x64::vzeroupper(buf);
        }
        if needs_frame {
            if frame_bytes > 0 {
                x64::alu_ri(buf, true, 0, RSP, frame_bytes); // add rsp, frame_bytes
            }
            for &r in saved_cs.iter().rev() {
                x64::pop_r64(buf, r);
            }
            x64::pop_r64(buf, RBP);
        }
    };

    // Helpers for loading/storing allocated values.
    let load_gpr = |buf: &mut Vec<u8, Alloc<'a>>, v: u32, scratch: XReg| -> Result<XReg, TierError> {
        match at_loc(&loc_of, to_idx(v)) {
            X64Loc::Gpr(r) => Ok(r),
            X64Loc::GprSpill(s) => {
                x64::load_disp(buf, 8, scratch, RSP, spill_off(spill_base, s));
                Ok(scratch)
            }
            X64Loc::None => {
                let cval = at_opt(&const_u64, to_idx(v)).ok_or(TierError::Refused {
                    tier: TIER,
                    inst: Some(v),
                    what: "unallocated x64 GPR",
                })?;
                x64::mov_imm64(buf, scratch, cval);
                Ok(scratch)
            }
            _ => Err(TierError::Refused { tier: TIER, inst: Some(v), what: "expected x64 GPR" }),
        }
    };
    let load_xmm = |buf: &mut Vec<u8, Alloc<'a>>, v: u32, double: bool, scratch: XReg| -> Result<XReg, TierError> {
        match at_loc(&loc_of, to_idx(v)) {
            X64Loc::Xmm(r) => Ok(r),
            X64Loc::XmmSpill(s) => {
                x64::load_fp_disp(buf, double, scratch, RSP, spill_off(spill_base, s));
                Ok(scratch)
            }
            _ => Err(TierError::Refused { tier: TIER, inst: Some(v), what: "expected x64 XMM" }),
        }
    };
    let dst_gpr = |inst: usize| -> Result<(XReg, Option<u32>), TierError> {
        match at_loc(&loc_of, inst) {
            X64Loc::Gpr(r) => Ok((r, None)),
            X64Loc::GprSpill(s) => Ok((R10, Some(s))),
            X64Loc::None => Ok((R10, None)),
            _ => Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "expected x64 GPR dst" }),
        }
    };
    let dst_xmm = |inst: usize| -> Result<(XReg, Option<u32>), TierError> {
        match at_loc(&loc_of, inst) {
            X64Loc::Xmm(r) => Ok((r, None)),
            X64Loc::XmmSpill(s) => Ok((XMM14, Some(s))),
            X64Loc::None => Ok((XMM14, None)),
            _ => Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "expected x64 XMM dst" }),
        }
    };
    let load_vec = |buf: &mut Vec<u8, Alloc<'a>>, v: u32, scratch: XReg| -> Result<XReg, TierError> {
        match at_loc(&loc_of, to_idx(v)) {
            X64Loc::Xmm(r) => Ok(r),
            X64Loc::VecSpill(s) => {
                x64::movdqu_load(buf, scratch, RSP, spill_off(spill_base, s));
                Ok(scratch)
            }
            X64Loc::Vec256Spill(s) => {
                x64::vmovdqu_load(buf, true, scratch, RSP, spill_off(spill_base, s));
                Ok(scratch)
            }
            X64Loc::Vec512Spill(s) => {
                x64::avx512_vmovdqu64_load(buf, scratch, RSP, spill_off(spill_base, s));
                Ok(scratch)
            }
            _ => Err(TierError::Refused { tier: TIER, inst: Some(v), what: "expected x64 Vec" }),
        }
    };
    let dst_vec = |inst: usize| -> Result<(XReg, Option<(u32, VecW)>), TierError> {
        match at_loc(&loc_of, inst) {
            X64Loc::Xmm(r) => Ok((r, None)),
            X64Loc::VecSpill(s) => Ok((XMM14, Some((s, VecW::W128)))),
            X64Loc::Vec256Spill(s) => Ok((XMM14, Some((s, VecW::W256)))),
            X64Loc::Vec512Spill(s) => Ok((XMM14, Some((s, VecW::W512)))),
            X64Loc::None => Ok((XMM14, None)),
            _ => Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "expected x64 Vec dst" }),
        }
    };
    let store_vec_spill = |buf: &mut Vec<u8, Alloc<'a>>, r: XReg, sp: Option<(u32, VecW)>| {
        if let Some((s, vw)) = sp {
            match vw {
                VecW::W512 => x64::avx512_vmovdqu64_store(buf, r, RSP, spill_off(spill_base, s)),
                VecW::W256 => x64::vmovdqu_store(buf, true, r, RSP, spill_off(spill_base, s)),
                VecW::W128 => x64::movdqu_store(buf, r, RSP, spill_off(spill_base, s)),
            }
        }
    };
    let narrow_x64 = |buf: &mut Vec<u8, Alloc<'a>>, r: XReg, bits: u32| {
        match bits {
            1 => x64::alu_ri(buf, false, 4, r, 1), // and r32, 1
            8 => x64::movzx_b(buf, r, r),
            16 => x64::movzx_w(buf, r, r),
            32 => x64::mov_rr(buf, false, r, r),
            _ => {}
        }
    };

    // Marshal SysV entry arguments into allocated regs/spills.
    let mut gpr_moves: Vec<(X64Loc, X64Loc), Alloc<'a>> = Vec::with_capacity_in(16, alloc);
    let mut xmm_moves: Vec<(X64Loc, X64Loc), Alloc<'a>> = Vec::with_capacity_in(16, alloc);
    let mut const_writes: Vec<(X64Loc, u64), Alloc<'a>> = Vec::with_capacity_in(8, alloc);
    {
        let (mut gi, mut fi) = (0usize, 0usize);
        for p in entry_params.start..entry_params.end {
            let ty = ty_at(view.tys, p);
            let dst = at_loc(&loc_of, p);
            if is_fp_ty(ty).is_some() {
                if let Some(&reg) = SYSV_ARG_FP.get(fi) {
                    let src = X64Loc::Xmm(reg);
                    fi += 1;
                    if dst != X64Loc::None {
                        xmm_moves.push((dst, src));
                    }
                }
            } else if let Some(&reg) = SYSV_ARG_GPR.get(gi) {
                let src = X64Loc::Gpr(reg);
                gi += 1;
                if dst != X64Loc::None {
                    gpr_moves.push((dst, src));
                }
            }
        }
        emit_parallel_x64_gpr(&mut code, &mut gpr_moves, spill_base);
        emit_parallel_x64_xmm(&mut code, &mut xmm_moves, spill_base, has_ymm);
        for p in entry_params {
            if let X64Loc::Gpr(r) = at_loc(&loc_of, p) {
                let ty = ty_at(view.tys, p);
                if is_fp_ty(ty).is_none() && ty.bits(64) < 64 {
                    narrow_x64(&mut code, r, ty.bits(64));
                }
            }
        }
    }

    let emit_cmp_x64 = |buf: &mut Vec<u8, Alloc<'a>>, cond: IntCond, a: u32, bv: u32, op_ty: Ty| -> Result<(), TierError> {
        let bits = op_ty.bits(64);
        let ra = load_gpr(buf, a, R10)?;
        let signed = matches!(cond, IntCond::Slt | IntCond::Sle | IntCond::Sgt | IntCond::Sge);
        if (bits == 64 || bits == 32) && at_opt(&const_u64, to_idx(bv)) == Some(0) && matches!(cond, IntCond::Eq | IntCond::Ne) {
            x64::test_rr(buf, bits == 64, ra, ra);
        } else if (bits == 64 || bits == 32)
            && let Some(imm) = at_opt(&const_u64, to_idx(bv)).and_then(|v| i32::try_from(v.cast_signed()).ok())
        {
            x64::alu_ri(buf, bits == 64, 7, ra, imm); // cmp ra, imm32
        } else if signed && bits < 32 {
            let rb = load_gpr(buf, bv, R11)?;
            match bits {
                8 => {
                    x64::movsx_b(buf, R10, ra);
                    x64::movsx_b(buf, R11, rb);
                }
                16 => {
                    x64::movsx_w(buf, R10, ra);
                    x64::movsx_w(buf, R11, rb);
                }
                _ => {
                    x64::mov_rr(buf, true, R10, ra);
                    x64::mov_rr(buf, true, R11, rb);
                }
            }
            x64::alu_rr(buf, true, 0x39, R10, R11);
        } else {
            let rb = load_gpr(buf, bv, R11)?;
            x64::alu_rr(buf, bits != 32, 0x39, ra, rb);
        }
        Ok(())
    };

    let emit_edge_x64 = |buf: &mut Vec<u8, Alloc<'a>>,
                         gpr_m: &mut Vec<(X64Loc, X64Loc), Alloc<'a>>,
                         xmm_m: &mut Vec<(X64Loc, X64Loc), Alloc<'a>>,
                         cw: &mut Vec<(X64Loc, u64), Alloc<'a>>,
                         tb: u32,
                         astart: usize,
                         alen: usize|
     -> Result<(), TierError> {
        let tp = view.params(Block(tb));
        gpr_m.clear();
        xmm_m.clear();
        cw.clear();
        for k in 0..alen {
            let dst = at_loc(&loc_of, tp.start + k);
            if dst == X64Loc::None {
                continue;
            }
            let sv = at_u32(view.extra, astart + k);
            let src = at_loc(&loc_of, to_idx(sv));
            match dst {
                X64Loc::Gpr(_) | X64Loc::GprSpill(_) => {
                    if src == X64Loc::None {
                        let cval = at_opt(&const_u64, to_idx(sv)).ok_or(TierError::Refused {
                            tier: TIER,
                            inst: Some(sv),
                            what: "missing const edge arg",
                        })?;
                        cw.push((dst, cval));
                    } else {
                        gpr_m.push((dst, src));
                    }
                }
                X64Loc::Xmm(_) | X64Loc::XmmSpill(_) | X64Loc::VecSpill(_) | X64Loc::Vec256Spill(_) | X64Loc::Vec512Spill(_) => xmm_m.push((dst, src)),
                X64Loc::None => {}
            }
        }
        emit_parallel_x64_gpr(buf, gpr_m, spill_base);
        emit_parallel_x64_xmm(buf, xmm_m, spill_base, has_ymm);
        for &(dst, val) in cw.iter() {
            match dst {
                X64Loc::Gpr(r) => x64::mov_imm64(buf, r, val),
                X64Loc::GprSpill(s) => {
                    x64::mov_imm64(buf, R10, val);
                    x64::store_disp(buf, 8, R10, RSP, spill_off(spill_base, s));
                }
                _ => {}
            }
        }
        Ok(())
    };

    // Emit blocks in hot-first order (cold/trap blocks sunk; loop headers 16B-aligned with optimal NOPs)!
    for (order_idx, &b) in block_order.iter().enumerate() {
        let next_block = block_order.get(order_idx + 1).copied();
        if b != 0 && at_bool(&is_loop_header, b) {
            let rem = code.len() % 16;
            if rem != 0 {
                x64::emit_nops(&mut code, 16 - rem);
            }
        }
        set_at(&mut block_byte_off, b, code.len());
        let range = view.block(Block(u32_of(b)));
        let mut inst = range.start;
        while inst < range.end {
            if at_bool(&skip_emit, inst) {
                inst += 1;
                continue;
            }
            let op = view.op(inst);
            let ty = ty_at(view.tys, inst);
            let fp_ty = is_fp_ty(ty);
            let vec_ty = is_x64_vec_ty(ty, has_avx2, has_avx512);
            let mask_ty = is_vec_mask_ty(ty);
            let bits = if ty == Ty::NONE || fp_ty.is_some() || vec_ty.is_some() || mask_ty.is_some() { 0 } else { ty.bits(64) };
            let loc = at_loc(&loc_of, inst);
            let has_side_effect = op.is_terminator()
                || matches!(
                    op,
                    Op::Call
                        | Op::CallIndirect
                        | Op::Load
                        | Op::Store
                        | Op::AtomicLoad
                        | Op::AtomicStore
                        | Op::AtomicRmw
                        | Op::Fence
                        | Op::Trap
                        | Op::Udiv
                        | Op::Sdiv
                        | Op::Urem
                        | Op::Srem
                );
            if !has_side_effect && loc == X64Loc::None {
                inst += 1;
                continue;
            }

            // 128-bit SSE2 & 256-bit AVX2 vector lane-mask comparison (`Icmp` -> `<lanes x i1>`).
            if mask_ty.is_some() && op == Op::Icmp {
                let at = to_idx(at_u32(view.data, inst));
                let cond = IntCond::from_word(at_u32(view.extra, at)).ok_or(TierError::Refused {
                    tier: TIER,
                    inst: Some(u32_of(inst)),
                    what: "invalid x86-64 vec icmp cond",
                })?;
                let a_v = at_u32(view.extra, at + 1);
                let b_v = at_u32(view.extra, at + 2);
                let (vsize, _, vw) = is_x64_vec_ty(ty_at(view.tys, to_idx(a_v)), has_avx2, has_avx512).ok_or(
                    TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "non-vector x86-64 mask icmp" },
                )?;
                let l256 = vw == VecW::W256;
                let (vd, sp) = dst_vec(inst)?;
                let mut va = load_vec(&mut code, a_v, XMM14)?;
                let mut vb = load_vec(&mut code, b_v, XMM15)?;
                let unsigned = matches!(cond, IntCond::Ult | IntCond::Ule | IntCond::Ugt | IntCond::Uge);
                if unsigned {
                    // Bias both operands by sign bit (`1 << (elem_bits - 1)`) so signed `pcmpgt` gives exact unsigned order!
                    let sign_bit = 1u64 << ((8u32 << vsize) - 1);
                    x64::mov_imm64(&mut code, R10, sign_bit);
                    if has_avx2 {
                        x64::mov_gpr_to_xmm(&mut code, vsize == 3, XMM14, R10);
                        x64::avx2_vpbroadcast(&mut code, l256, vsize, XMM14, XMM14);
                        if va == XMM14 {
                            // `a_v` was spilled into XMM14: reload into XMM15 first.
                            let va2 = load_vec(&mut code, a_v, XMM15)?;
                            x64::avx_vec_int_bin_w(&mut code, l256, 0xef, vd, va2, XMM14);
                            let vb2 = load_vec(&mut code, b_v, XMM15)?;
                            x64::avx_vec_int_bin_w(&mut code, l256, 0xef, XMM15, vb2, XMM14);
                            va = vd;
                            vb = XMM15;
                        } else if vb == XMM14 {
                            x64::avx_vec_int_bin_w(&mut code, l256, 0xef, vd, va, XMM14);
                            let vb2 = load_vec(&mut code, b_v, XMM15)?;
                            x64::avx_vec_int_bin_w(&mut code, l256, 0xef, XMM15, vb2, XMM14);
                            va = vd;
                            vb = XMM15;
                        } else {
                            x64::avx_vec_int_bin_w(&mut code, l256, 0xef, vd, va, XMM14);
                            x64::avx_vec_int_bin_w(&mut code, l256, 0xef, XMM15, vb, XMM14);
                            va = vd;
                            vb = XMM15;
                        }
                    } else {
                        // Baseline SSE2 sign-bit splat into XMM15 (only `vsize < 3` allowed in check_x64_subset).
                        let mask32 = match vsize {
                            0 => 0x8080_8080u64,
                            1 => 0x8000_8000u64,
                            _ => 0x8000_0000u64,
                        };
                        x64::mov_imm64(&mut code, R10, mask32);
                        x64::mov_gpr_to_xmm(&mut code, false, XMM15, R10);
                        x64::pshufd(&mut code, XMM15, XMM15, 0x00);
                        if va != XMM14 {
                            x64::movdqa_rr(&mut code, XMM14, va);
                        }
                        x64::vec_int_bin(&mut code, 0xef, XMM14, XMM15);
                        if vb != vd {
                            x64::movdqa_rr(&mut code, vd, vb);
                        }
                        x64::vec_int_bin(&mut code, 0xef, vd, XMM15);
                        va = XMM14;
                        vb = vd;
                    }
                }
                let (is_eq, swap, invert) = match cond {
                    IntCond::Eq => (true, false, false),
                    IntCond::Ne => (true, false, true),
                    IntCond::Sgt | IntCond::Ugt => (false, false, false),
                    IntCond::Slt | IntCond::Ult => (false, true, false),
                    IntCond::Sle | IntCond::Ule => (false, false, true), // !(a > b)
                    IntCond::Sge | IntCond::Uge => (false, true, true),  // !(b > a)
                };
                let (lhs, rhs) = if swap { (vb, va) } else { (va, vb) };
                if has_avx2 {
                    if vsize == 3 {
                        x64::avx_vec_int_0f38(&mut code, l256, if is_eq { 0x29 } else { 0x37 }, vd, lhs, rhs);
                    } else {
                        let opc = if is_eq {
                            match vsize {
                                0 => 0x74,
                                1 => 0x75,
                                _ => 0x76,
                            }
                        } else {
                            match vsize {
                                0 => 0x64,
                                1 => 0x65,
                                _ => 0x66,
                            }
                        };
                        x64::avx_vec_int_bin_w(&mut code, l256, opc, vd, lhs, rhs);
                    }
                    if invert {
                        x64::avx_vec_int_bin_w(&mut code, l256, 0x76, XMM15, XMM15, XMM15); // all ones
                        x64::avx_vec_int_bin_w(&mut code, l256, 0xef, vd, vd, XMM15);
                    }
                } else {
                    let opc = if is_eq {
                        match vsize {
                            0 => 0x74,
                            1 => 0x75,
                            _ => 0x76,
                        }
                    } else {
                        match vsize {
                            0 => 0x64,
                            1 => 0x65,
                            _ => 0x66,
                        }
                    };
                    if vd == rhs && vd != lhs {
                        x64::movdqa_rr(&mut code, XMM15, rhs);
                        x64::movdqa_rr(&mut code, vd, lhs);
                        x64::vec_int_bin(&mut code, opc, vd, XMM15);
                    } else {
                        if vd != lhs {
                            x64::movdqa_rr(&mut code, vd, lhs);
                        }
                        x64::vec_int_bin(&mut code, opc, vd, rhs);
                    }
                    if invert {
                        x64::vec_int_bin(&mut code, 0x76, XMM15, XMM15);
                        x64::vec_int_bin(&mut code, 0xef, vd, XMM15);
                    }
                }
                store_vec_spill(&mut code, vd, sp);
                inst += 1;
                continue;
            }

            // 128-bit SSE2, 256-bit AVX2 (`ymm`), and 512-bit AVX-512 (`zmm`, 256B/iter!) vector fast path!
            if let Some((vsize, vfp, vw)) = vec_ty {
                let l256 = vw == VecW::W256;
                let l512 = vw == VecW::W512;
                match op {
                    Op::Param => {}
                    Op::Uninit => {
                        let (vd, sp) = dst_vec(inst)?;
                        if l512 {
                            x64::avx512_int_bin(&mut code, false, 0xef, vd, vd, vd);
                        } else if has_avx2 {
                            x64::avx_vec_int_bin_w(&mut code, l256, 0xef, vd, vd, vd);
                        } else {
                            x64::vec_int_bin(&mut code, 0xef, vd, vd);
                        }
                        store_vec_spill(&mut code, vd, sp);
                    }
                    Op::Freeze => {
                        let (vd, sp) = dst_vec(inst)?;
                        let vs = load_vec(&mut code, at_u32(view.data, inst), XMM14)?;
                        if vd != vs {
                            if l512 {
                                x64::avx512_int_bin(&mut code, false, 0xeb, vd, vs, vs);
                            } else if has_avx2 {
                                x64::vmovdqa_rr(&mut code, l256, vd, vs);
                            } else {
                                x64::movdqa_rr(&mut code, vd, vs);
                            }
                        }
                        store_vec_spill(&mut code, vd, sp);
                    }
                    Op::Splat => {
                        let (vd, sp) = dst_vec(inst)?;
                        let src_v = at_u32(view.data, inst);
                        if !vfp && at_opt(&const_u64, to_idx(src_v)) == Some(0) {
                            if l512 {
                                x64::avx512_int_bin(&mut code, false, 0xef, vd, vd, vd);
                            } else if has_avx2 {
                                x64::avx_vec_int_bin_w(&mut code, l256, 0xef, vd, vd, vd);
                            } else {
                                x64::vec_int_bin(&mut code, 0xef, vd, vd);
                            }
                        } else if has_avx2 {
                            if vfp {
                                let vs = load_xmm(&mut code, src_v, vsize == 3, XMM14)?;
                                x64::avx2_vpbroadcast(&mut code, l256, vsize, vd, vs);
                            } else {
                                let rs = load_gpr(&mut code, src_v, R10)?;
                                x64::mov_gpr_to_xmm(&mut code, vsize == 3, vd, rs);
                                x64::avx2_vpbroadcast(&mut code, l256, vsize, vd, vd);
                            }
                        } else if vfp {
                            let vs = load_xmm(&mut code, src_v, vsize == 3, XMM14)?;
                            if vd != vs {
                                x64::movdqa_rr(&mut code, vd, vs);
                            }
                            if vsize == 3 {
                                x64::punpcklqdq(&mut code, vd, vd);
                            } else {
                                x64::pshufd(&mut code, vd, vd, 0x00);
                            }
                        } else {
                            let rs = load_gpr(&mut code, src_v, R10)?;
                            if vsize == 3 {
                                x64::mov_gpr_to_xmm(&mut code, true, vd, rs);
                                x64::punpcklqdq(&mut code, vd, vd);
                            } else if vsize == 2 {
                                x64::mov_gpr_to_xmm(&mut code, false, vd, rs);
                                x64::pshufd(&mut code, vd, vd, 0x00);
                            } else if vsize == 1 {
                                x64::movzx_w(&mut code, R10, rs);
                                x64::mov_rr(&mut code, false, R11, R10);
                                x64::shift_ri(&mut code, false, 4, R11, 16);
                                x64::alu_rr(&mut code, false, 0x09, R10, R11);
                                x64::mov_gpr_to_xmm(&mut code, false, vd, R10);
                                x64::pshufd(&mut code, vd, vd, 0x00);
                            } else {
                                x64::movzx_b(&mut code, R10, rs);
                                x64::imul_rri(&mut code, false, R10, R10, 0x0101_0101i32);
                                x64::mov_gpr_to_xmm(&mut code, false, vd, R10);
                                x64::pshufd(&mut code, vd, vd, 0x00);
                            }
                        }
                        store_vec_spill(&mut code, vd, sp);
                    }
                    Op::Load => {
                        let (vd, sp) = dst_vec(inst)?;
                        let emit_vld = |buf: &mut Vec<u8, Alloc<'a>>, base: XReg, off: i32| {
                            if l512 {
                                x64::avx512_vmovdqu64_load(buf, vd, base, off);
                            } else if has_avx2 {
                                x64::vmovdqu_load(buf, l256, vd, base, off);
                            } else {
                                x64::movdqu_load(buf, vd, base, off);
                            }
                        };
                        match at_fusion(&fusion, inst) {
                            X64Fusion::MemRspOff { off } => emit_vld(&mut code, RSP, off),
                            X64Fusion::MemBaseOff { base, off, .. } => {
                                let rb = load_gpr(&mut code, base, R11)?;
                                emit_vld(&mut code, rb, off);
                            }
                            _ => {
                                let at = to_idx(at_u32(view.data, inst));
                                let rb = load_gpr(&mut code, at_u32(view.extra, at), R11)?;
                                let off = at_u32(view.extra, at + 1).cast_signed();
                                emit_vld(&mut code, rb, off);
                            }
                        }
                        store_vec_spill(&mut code, vd, sp);
                    }
                    Op::Add | Op::Sub | Op::Mul | Op::And | Op::Or | Op::Xor => {
                        let at = to_idx(at_u32(view.data, inst));
                        let (vd, sp) = dst_vec(inst)?;
                        let va = load_vec(&mut code, at_u32(view.extra, at), XMM14)?;
                        let vb = load_vec(&mut code, at_u32(view.extra, at + 1), XMM15)?;
                        if op == Op::Mul && vsize == 2 {
                            x64::avx_vec_int_0f38(&mut code, l256, 0x40, vd, va, vb); // vpmulld
                        } else {
                            let opc = match op {
                                Op::Add => match vsize {
                                    0 => 0xfc,
                                    1 => 0xfd,
                                    2 => 0xfe,
                                    _ => 0xd4,
                                },
                                Op::Sub => match vsize {
                                    0 => 0xf8,
                                    1 => 0xf9,
                                    2 => 0xfa,
                                    _ => 0xfb,
                                },
                                Op::Mul => 0xd5, // pmullw
                                Op::And => 0xdb,
                                Op::Or => 0xeb,
                                Op::Xor => 0xef,
                                _ => {
                                    return Err(TierError::Refused {
                                        tier: TIER,
                                        inst: Some(u32_of(inst)),
                                        what: "unsupported x86-64 vector int op",
                                    });
                                }
                            };
                            if l512 {
                                let w64 = matches!(op, Op::Add | Op::Sub) && vsize == 3;
                                x64::avx512_int_bin(&mut code, w64, opc, vd, va, vb);
                            } else if has_avx2 {
                                x64::avx_vec_int_bin_w(&mut code, l256, opc, vd, va, vb);
                            } else if vd == vb && vd != va {
                                x64::movdqa_rr(&mut code, XMM15, vb);
                                x64::movdqa_rr(&mut code, vd, va);
                                x64::vec_int_bin(&mut code, opc, vd, XMM15);
                            } else {
                                if vd != va {
                                    x64::movdqa_rr(&mut code, vd, va);
                                }
                                x64::vec_int_bin(&mut code, opc, vd, vb);
                            }
                        }
                        store_vec_spill(&mut code, vd, sp);
                    }
                    Op::Select => {
                        let at = to_idx(at_u32(view.data, inst));
                        let (cv, tv, fv) = (at_u32(view.extra, at), at_u32(view.extra, at + 1), at_u32(view.extra, at + 2));
                        let (vd, sp) = dst_vec(inst)?;
                        let fvi = to_idx(fv);
                        let is_zero_splat = view.op(fvi) == Op::Splat
                            && at_opt(&const_u64, to_idx(at_u32(view.data, fvi))) == Some(0);
                        if is_zero_splat {
                            let vm = load_vec(&mut code, cv, XMM14)?;
                            let vt = load_vec(&mut code, tv, XMM15)?;
                            if has_avx2 {
                                x64::avx_vec_int_bin_w(&mut code, l256, 0xdb, vd, vt, vm); // vpand
                            } else if vd == vm && vd != vt {
                                x64::vec_int_bin(&mut code, 0xdb, vd, vt);
                            } else {
                                if vd != vt {
                                    x64::movdqa_rr(&mut code, vd, vt);
                                }
                                x64::vec_int_bin(&mut code, 0xdb, vd, vm);
                            }
                        } else if has_avx2 {
                            let vm = load_vec(&mut code, cv, XMM14)?;
                            let vt = load_vec(&mut code, tv, XMM15)?;
                            let vf = match at_loc(&loc_of, fvi) {
                                X64Loc::Xmm(r) => r,
                                _ => {
                                    let dst_f = if vd != vm && vd != vt { vd } else { XMM14 };
                                    load_vec(&mut code, fv, dst_f)?
                                }
                            };
                            x64::avx2_vpblendvb(&mut code, l256, vd, vf, vt, vm);
                        } else {
                            let vm = load_vec(&mut code, cv, XMM14)?;
                            let vt = load_vec(&mut code, tv, XMM15)?;
                            let vf = match at_loc(&loc_of, fvi) {
                                X64Loc::Xmm(r) => r,
                                _ => load_vec(&mut code, fv, vd)?,
                            };
                            x64::movdqa_rr(&mut code, XMM14, vm);
                            x64::vec_int_bin(&mut code, 0xdf, XMM14, vf); // pandn XMM14 = (~mask) & vf
                            x64::movdqa_rr(&mut code, XMM15, vt);
                            x64::vec_int_bin(&mut code, 0xdb, XMM15, vm); // pand XMM15 = vt & mask
                            x64::movdqa_rr(&mut code, vd, XMM15);
                            x64::vec_int_bin(&mut code, 0xeb, vd, XMM14); // por
                        }
                        store_vec_spill(&mut code, vd, sp);
                    }
                    Op::Fadd | Op::Fsub | Op::Fmul | Op::Fma | Op::Fmuladd => {
                        let at = to_idx(at_u32(view.data, inst));
                        let (vd, sp) = dst_vec(inst)?;
                        let double = vsize == 3;
                        let fma_ops = match (op, at_fusion(&fusion, inst)) {
                            (Op::Fma | Op::Fmuladd, _) => {
                                Some((at_u32(view.extra, at), at_u32(view.extra, at + 1), at_u32(view.extra, at + 2)))
                            }
                            (Op::Fadd, X64Fusion::Fma { a, b, acc }) => Some((a, b, acc)),
                            _ => None,
                        };
                        if let Some((a_v, b_v, c_v)) = fma_ops {
                            let loc_a = at_loc(&loc_of, to_idx(a_v));
                            let loc_b = at_loc(&loc_of, to_idx(b_v));
                            let loc_c = at_loc(&loc_of, to_idx(c_v));
                            let b_spill = match loc_b {
                                X64Loc::VecSpill(sb) | X64Loc::Vec256Spill(sb) => Some(sb),
                                _ => None,
                            };
                            if let (Some(sb), true) = (b_spill, sp.is_some() && !matches!(loc_a, X64Loc::Xmm(_)) && !matches!(loc_c, X64Loc::Xmm(_))) {
                                let vc = load_vec(&mut code, c_v, XMM14)?;
                                if vc != vd {
                                    x64::vmovdqa_rr(&mut code, l256, vd, vc);
                                }
                                let va = load_vec(&mut code, a_v, XMM15)?;
                                x64::fma3_vfmadd231_mem_w(&mut code, l256, double, true, vd, va, RSP, spill_off(spill_base, sb));
                            } else {
                                let va = load_vec(&mut code, a_v, XMM14)?;
                                let vb = load_vec(&mut code, b_v, XMM15)?;
                                let vc = match loc_c {
                                    X64Loc::Xmm(r) => r,
                                    _ => {
                                        let dst_c = if vd != va && vd != vb { vd } else if va != XMM14 && vb != XMM14 { XMM14 } else { XMM15 };
                                        load_vec(&mut code, c_v, dst_c)?
                                    }
                                };
                                if vd == vc {
                                    x64::fma3_vfmadd231_w(&mut code, l256, double, true, vd, va, vb);
                                } else if vd == va {
                                    x64::fma3_vfmadd213_w(&mut code, l256, double, true, vd, vb, vc);
                                } else if vd == vb {
                                    x64::fma3_vfmadd213_w(&mut code, l256, double, true, vd, va, vc);
                                } else {
                                    if has_avx2 {
                                        x64::vmovdqa_rr(&mut code, l256, vd, vc);
                                    } else {
                                        x64::movdqa_rr(&mut code, vd, vc);
                                    }
                                    x64::fma3_vfmadd231_w(&mut code, l256, double, true, vd, va, vb);
                                }
                            }
                        } else {
                            let va = load_vec(&mut code, at_u32(view.extra, at), XMM14)?;
                            let vb = load_vec(&mut code, at_u32(view.extra, at + 1), XMM15)?;
                            let opc = match op {
                                Op::Fadd => 0x58u8,
                                Op::Fmul => 0x59,
                                Op::Fsub => 0x5c,
                                _ => {
                                    return Err(TierError::Refused {
                                        tier: TIER,
                                        inst: Some(u32_of(inst)),
                                        what: "unsupported x86-64 vector fp op",
                                    });
                                }
                            };
                            if has_avx2 {
                                x64::avx_vec_fp_bin_w(&mut code, l256, double, opc, vd, va, vb);
                            } else if vd == vb && vd != va {
                                x64::movdqa_rr(&mut code, XMM15, vb);
                                x64::movdqa_rr(&mut code, vd, va);
                                x64::vec_fp_bin(&mut code, double, opc, vd, XMM15);
                            } else {
                                if vd != va {
                                    x64::movdqa_rr(&mut code, vd, va);
                                }
                                x64::vec_fp_bin(&mut code, double, opc, vd, vb);
                            }
                        }
                        store_vec_spill(&mut code, vd, sp);
                    }
                    _ => {
                        return Err(TierError::Refused {
                            tier: TIER,
                            inst: Some(u32_of(inst)),
                            what: "unsupported x86-64 vector op",
                        });
                    }
                }
                inst += 1;
                continue;
            }
            if op == Op::Store {
                let at = to_idx(at_u32(view.data, inst));
                let st_ty = ty_at(view.tys, to_idx(at_u32(view.extra, at)));
                if let Some((_, _, vw)) = is_x64_vec_ty(st_ty, has_avx2, has_avx512) {
                    let vs = load_vec(&mut code, at_u32(view.extra, at), XMM14)?;
                    let emit_vst = |buf: &mut Vec<u8, Alloc<'a>>, base: XReg, off: i32| {
                        match vw {
                            VecW::W512 => x64::avx512_vmovdqu64_store(buf, vs, base, off),
                            VecW::W256 => x64::vmovdqu_store(buf, true, vs, base, off),
                            VecW::W128 => {
                                if has_avx2 {
                                    x64::vmovdqu_store(buf, false, vs, base, off);
                                } else {
                                    x64::movdqu_store(buf, vs, base, off);
                                }
                            }
                        }
                    };
                    match at_fusion(&fusion, inst) {
                        X64Fusion::MemRspOff { off } => emit_vst(&mut code, RSP, off),
                        X64Fusion::MemBaseOff { base, off, .. } => {
                            let rb = load_gpr(&mut code, base, R11)?;
                            emit_vst(&mut code, rb, off);
                        }
                        _ => {
                            let rb = load_gpr(&mut code, at_u32(view.extra, at + 1), R11)?;
                            let off = at_u32(view.extra, at + 2).cast_signed();
                            emit_vst(&mut code, rb, off);
                        }
                    }
                    inst += 1;
                    continue;
                }
            }
            if matches!(op, Op::ReduceAdd | Op::ReduceAnd | Op::ReduceOr | Op::ReduceXor | Op::ReduceUmax | Op::ReduceFadd) {
                if op == Op::ReduceFadd {
                    let at = to_idx(at_u32(view.data, inst));
                    let start_v = at_u32(view.extra, at);
                    let vec_v = at_u32(view.extra, at + 1);
                    let (vsize, _, vw) = is_x64_vec_ty(ty_at(view.tys, to_idx(vec_v)), has_avx2, has_avx512).ok_or(
                        TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "non-vector x86-64 reduce_fadd" },
                    )?;
                    let l256 = vw == VecW::W256;
                    let double = vsize == 3;
                    let (vd, sp) = dst_xmm(inst)?;
                    let vs = load_vec(&mut code, vec_v, XMM14)?;
                    let vstart = load_xmm(&mut code, start_v, double, XMM15)?;
                    if l256 {
                        x64::avx2_vextracti128(&mut code, XMM15, vs, 1);
                        x64::avx_vec_fp_bin_w(&mut code, false, double, 0x58, XMM14, vs, XMM15);
                    } else {
                        x64::movdqa_rr(&mut code, XMM14, vs);
                    }
                    if double {
                        x64::pshufd(&mut code, XMM15, XMM14, 0x4e); // swap high/low 64b
                        x64::fp_bin(&mut code, true, 0x58, XMM14, XMM15);
                    } else {
                        x64::pshufd(&mut code, XMM15, XMM14, 0x4e);
                        x64::vec_fp_bin(&mut code, false, 0x58, XMM14, XMM15);
                        x64::pshufd(&mut code, XMM15, XMM14, 0xb1);
                        x64::fp_bin(&mut code, false, 0x58, XMM14, XMM15);
                    }
                    if vd != vstart {
                        x64::movfp_rr(&mut code, double, vd, vstart);
                    }
                    x64::fp_bin(&mut code, double, 0x58, vd, XMM14);
                    if let Some(s) = sp {
                        x64::store_fp_disp(&mut code, double, vd, RSP, spill_off(spill_base, s));
                    }
                } else {
                    let vec_v = at_u32(view.data, inst);
                    let (vsize, _, vw) = is_x64_vec_ty(ty_at(view.tys, to_idx(vec_v)), has_avx2, has_avx512).ok_or(
                        TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "non-vector x86-64 reduce" },
                    )?;
                    let (rd, sp) = dst_gpr(inst)?;
                    let vs = load_vec(&mut code, vec_v, XMM14)?;
                    let opc = match op {
                        Op::ReduceAdd => match vsize {
                            0 => 0xfc,
                            1 => 0xfd,
                            2 => 0xfe,
                            _ => 0xd4,
                        },
                        Op::ReduceXor => 0xef,
                        Op::ReduceOr => 0xeb,
                        Op::ReduceAnd => 0xdb,
                        Op::ReduceUmax => 0xde,
                        _ => {
                            return Err(TierError::Refused {
                                tier: TIER,
                                inst: Some(u32_of(inst)),
                                what: "unsupported x86-64 reduce op",
                            });
                        }
                    };
                    match vw {
                        VecW::W512 => {
                            // 512b -> 256b -> 128b horizontal AVX-512 reduction!
                            x64::avx512_vextracti64x4(&mut code, XMM15, vs, 1);
                            x64::avx_vec_int_bin_w(&mut code, true, opc, XMM14, vs, XMM15);
                            x64::avx2_vextracti128(&mut code, XMM15, XMM14, 1);
                            x64::avx_vec_int_bin_w(&mut code, false, opc, XMM14, XMM14, XMM15);
                        }
                        VecW::W256 => {
                            x64::avx2_vextracti128(&mut code, XMM15, vs, 1);
                            x64::avx_vec_int_bin_w(&mut code, false, opc, XMM14, vs, XMM15);
                        }
                        VecW::W128 => {
                            x64::movdqa_rr(&mut code, XMM14, vs);
                        }
                    }
                    let elem_bytes = 1u8 << vsize;
                    let mut step = 8u8;
                    while step >= elem_bytes {
                        x64::movdqa_rr(&mut code, XMM15, XMM14);
                        x64::psrldq(&mut code, XMM15, step);
                        x64::vec_int_bin(&mut code, opc, XMM14, XMM15);
                        step >>= 1;
                    }
                    x64::mov_xmm_to_gpr(&mut code, vsize == 3, rd, XMM14);
                    if vsize < 2 {
                        narrow_x64(&mut code, rd, 8 << vsize);
                    }
                    if let Some(s) = sp {
                        x64::store_disp(&mut code, 8, rd, RSP, spill_off(spill_base, s));
                    }
                }
                inst += 1;
                continue;
            }

            match op {
                Op::Param | Op::SlotLive | Op::SlotDead | Op::Assume => {}
                Op::Null | Op::Uninit => {
                    if fp_ty.is_some() {
                        let (vd, sp) = dst_xmm(inst)?;
                        x64::mov_imm64(&mut code, R10, 0);
                        x64::mov_gpr_to_xmm(&mut code, true, vd, R10);
                        if let Some(s) = sp {
                            x64::store_fp_disp(&mut code, true, vd, RSP, spill_off(spill_base, s));
                        }
                    } else {
                        let (rd, sp) = dst_gpr(inst)?;
                        x64::mov_imm64(&mut code, rd, 0);
                        if let Some(s) = sp {
                            x64::store_disp(&mut code, 8, rd, RSP, spill_off(spill_base, s));
                        }
                    }
                }
                Op::Iconst | Op::IconstWide => {
                    let (rd, sp) = dst_gpr(inst)?;
                    x64::mov_imm64(&mut code, rd, at_opt(&const_u64, inst).unwrap_or(0));
                    if let Some(s) = sp {
                        x64::store_disp(&mut code, 8, rd, RSP, spill_off(spill_base, s));
                    }
                }
                Op::Fconst => {
                    let (vd, sp) = dst_xmm(inst)?;
                    x64::mov_imm64(&mut code, R10, u64::from(at_u32(view.data, inst)));
                    x64::mov_gpr_to_xmm(&mut code, false, vd, R10);
                    if let Some(s) = sp {
                        x64::store_fp_disp(&mut code, false, vd, RSP, spill_off(spill_base, s));
                    }
                }
                Op::FconstWide => {
                    let (vd, sp) = dst_xmm(inst)?;
                    let at = to_idx(at_u32(view.data, inst));
                    let fb = u64::from(at_u32(view.extra, at)) | (u64::from(at_u32(view.extra, at + 1)) << 32);
                    x64::mov_imm64(&mut code, R10, fb);
                    x64::mov_gpr_to_xmm(&mut code, true, vd, R10);
                    if let Some(s) = sp {
                        x64::store_fp_disp(&mut code, true, vd, RSP, spill_off(spill_base, s));
                    }
                }
                Op::Freeze | Op::PtrAddr | Op::Expose | Op::FromExposed => {
                    if let Some(double) = fp_ty {
                        let (vd, sp) = dst_xmm(inst)?;
                        let vs = load_xmm(&mut code, at_u32(view.data, inst), double, XMM14)?;
                        if vd != vs {
                            x64::movfp_rr(&mut code, double, vd, vs);
                        }
                        if let Some(s) = sp {
                            x64::store_fp_disp(&mut code, double, vd, RSP, spill_off(spill_base, s));
                        }
                    } else {
                        let (rd, sp) = dst_gpr(inst)?;
                        let rs = load_gpr(&mut code, at_u32(view.data, inst), R10)?;
                        if rd != rs {
                            x64::mov_rr(&mut code, true, rd, rs);
                        }
                        if let Some(s) = sp {
                            x64::store_disp(&mut code, 8, rd, RSP, spill_off(spill_base, s));
                        }
                    }
                }
                Op::PtrWithAddr => {
                    let at = to_idx(at_u32(view.data, inst));
                    let (rd, sp) = dst_gpr(inst)?;
                    let rs = load_gpr(&mut code, at_u32(view.extra, at + 1), R10)?;
                    if rd != rs {
                        x64::mov_rr(&mut code, true, rd, rs);
                    }
                    if let Some(s) = sp {
                        x64::store_disp(&mut code, 8, rd, RSP, spill_off(spill_base, s));
                    }
                }
                Op::SlotAddr => {
                    let (rd, sp) = dst_gpr(inst)?;
                    x64::lea_base_disp(&mut code, true, rd, RSP, at_i32(&slot_at, to_idx(at_u32(view.data, inst))));
                    if let Some(s) = sp {
                        x64::store_disp(&mut code, 8, rd, RSP, spill_off(spill_base, s));
                    }
                }
                Op::PtrAdd => {
                    let at = to_idx(at_u32(view.data, inst));
                    let (ptr, by) = (at_u32(view.extra, at), at_u32(view.extra, at + 1));
                    let (rd, sp) = dst_gpr(inst)?;
                    let rp = load_gpr(&mut code, ptr, R10)?;
                    if let Some(imm) = at_opt(&const_u64, to_idx(by)).and_then(|v| i32::try_from(v.cast_signed()).ok()) {
                        x64::lea_base_disp(&mut code, true, rd, rp, imm);
                    } else {
                        let rb = load_gpr(&mut code, by, R11)?;
                        x64::lea_sib(&mut code, true, rd, rp, rb, 0, 0);
                    }
                    if let Some(s) = sp {
                        x64::store_disp(&mut code, 8, rd, RSP, spill_off(spill_base, s));
                    }
                }
                Op::Load => {
                    let (bytes, is_fp) = mem_bytes_of(ty).ok_or(TierError::Refused {
                        tier: TIER,
                        inst: Some(u32_of(inst)),
                        what: "unsupported x86-64 load type",
                    })?;
                    let double = bytes == 8;
                    let (dst, sp) = if is_fp { dst_xmm(inst)? } else { dst_gpr(inst)? };
                    match at_fusion(&fusion, inst) {
                        X64Fusion::MemRspOff { off } => {
                            if is_fp {
                                x64::load_fp_disp(&mut code, double, dst, RSP, off);
                            } else {
                                x64::load_disp(&mut code, bytes, dst, RSP, off);
                            }
                        }
                        X64Fusion::MemBaseOff { base, off, .. } => {
                            let rb = load_gpr(&mut code, base, R11)?;
                            if is_fp {
                                x64::load_fp_disp(&mut code, double, dst, rb, off);
                            } else {
                                x64::load_disp(&mut code, bytes, dst, rb, off);
                            }
                        }
                        X64Fusion::MemSib { base, idx, scale_log2, .. } => {
                            let rb = load_gpr(&mut code, base, R10)?;
                            let ri = load_gpr(&mut code, idx, R11)?;
                            if is_fp {
                                x64::lea_sib(&mut code, true, R11, rb, ri, scale_log2, 0);
                                x64::load_fp_disp(&mut code, double, dst, R11, 0);
                            } else {
                                x64::load_sib(&mut code, bytes, dst, rb, ri, scale_log2);
                            }
                        }
                        _ => {
                            let at = to_idx(at_u32(view.data, inst));
                            let rb = load_gpr(&mut code, at_u32(view.extra, at), R11)?;
                            let off = at_u32(view.extra, at + 1).cast_signed();
                            if is_fp {
                                x64::load_fp_disp(&mut code, double, dst, rb, off);
                            } else {
                                x64::load_disp(&mut code, bytes, dst, rb, off);
                            }
                        }
                    }
                    if let Some(s) = sp {
                        if is_fp {
                            x64::store_fp_disp(&mut code, double, dst, RSP, spill_off(spill_base, s));
                        } else {
                            x64::store_disp(&mut code, 8, dst, RSP, spill_off(spill_base, s));
                        }
                    }
                }
                Op::Store => {
                    let at = to_idx(at_u32(view.data, inst));
                    let val = at_u32(view.extra, at);
                    let (bytes, is_fp) = mem_bytes_of(ty_at(view.tys, to_idx(val))).ok_or(TierError::Refused {
                        tier: TIER,
                        inst: Some(u32_of(inst)),
                        what: "unsupported x86-64 store type",
                    })?;
                    let double = bytes == 8;
                    match at_fusion(&fusion, inst) {
                        X64Fusion::MemRspOff { off } => {
                            if is_fp {
                                let rv = load_xmm(&mut code, val, double, XMM14)?;
                                x64::store_fp_disp(&mut code, double, rv, RSP, off);
                            } else {
                                let rv = load_gpr(&mut code, val, R10)?;
                                x64::store_disp(&mut code, bytes, rv, RSP, off);
                            }
                        }
                        X64Fusion::MemBaseOff { base, off, .. } => {
                            let rb = load_gpr(&mut code, base, R11)?;
                            if is_fp {
                                let rv = load_xmm(&mut code, val, double, XMM14)?;
                                x64::store_fp_disp(&mut code, double, rv, rb, off);
                            } else {
                                let rv = load_gpr(&mut code, val, R10)?;
                                x64::store_disp(&mut code, bytes, rv, rb, off);
                            }
                        }
                        X64Fusion::MemSib { base, idx, scale_log2, .. } => {
                            let rb = load_gpr(&mut code, base, R10)?;
                            let ri = load_gpr(&mut code, idx, R11)?;
                            if is_fp {
                                let rv = load_xmm(&mut code, val, double, XMM14)?;
                                x64::lea_sib(&mut code, true, R11, rb, ri, scale_log2, 0);
                                x64::store_fp_disp(&mut code, double, rv, R11, 0);
                            } else if let X64Loc::Gpr(rv) = at_loc(&loc_of, to_idx(val)) {
                                // Single-instruction SIB scaled store: `mov [rb + ri*scale], rv`!
                                x64::store_sib(&mut code, bytes, rv, rb, ri, scale_log2);
                            } else {
                                x64::lea_sib(&mut code, true, R11, rb, ri, scale_log2, 0);
                                let rv = load_gpr(&mut code, val, R10)?;
                                x64::store_disp(&mut code, bytes, rv, R11, 0);
                            }
                        }
                        _ => {
                            let rb = load_gpr(&mut code, at_u32(view.extra, at + 1), R11)?;
                            let off = at_u32(view.extra, at + 2).cast_signed();
                            if is_fp {
                                let rv = load_xmm(&mut code, val, double, XMM14)?;
                                x64::store_fp_disp(&mut code, double, rv, rb, off);
                            } else {
                                let rv = load_gpr(&mut code, val, R10)?;
                                x64::store_disp(&mut code, bytes, rv, rb, off);
                            }
                        }
                    }
                }
                Op::AtomicLoad => {
                    let (bytes, _) = mem_bytes_of(ty).ok_or(TierError::Refused {
                        tier: TIER,
                        inst: Some(u32_of(inst)),
                        what: "unsupported x86-64 atomic load type",
                    })?;
                    let (rd, sp) = dst_gpr(inst)?;
                    let rp = load_gpr(&mut code, at_u32(view.data, inst), R11)?;
                    // x86-64 TSO: aligned loads are acquire/seq_cst automatically!
                    x64::load_disp(&mut code, bytes, rd, rp, 0);
                    if let Some(s) = sp {
                        x64::store_disp(&mut code, 8, rd, RSP, spill_off(spill_base, s));
                    }
                }
                Op::AtomicStore => {
                    let at = to_idx(at_u32(view.data, inst));
                    let val = at_u32(view.extra, at);
                    let ptr = at_u32(view.extra, at + 1);
                    let (bytes, _) = mem_bytes_of(ty_at(view.tys, to_idx(val))).ok_or(TierError::Refused {
                        tier: TIER,
                        inst: Some(u32_of(inst)),
                        what: "unsupported x86-64 atomic store type",
                    })?;
                    let rv = load_gpr(&mut code, val, R10)?;
                    let rp = load_gpr(&mut code, ptr, R11)?;
                    x64::store_disp(&mut code, bytes, rv, rp, 0);
                    if Flags(at_u8(view.flags, inst)).ordering() == Some(Ordering::SeqCst) {
                        x64::mfence(&mut code);
                    }
                }
                Op::AtomicRmw => {
                    let at = to_idx(at_u32(view.data, inst));
                    let kind = AtomicRmw::from_word(at_u32(view.extra, at)).ok_or(TierError::Refused {
                        tier: TIER,
                        inst: Some(u32_of(inst)),
                        what: "invalid x86-64 atomic_rmw kind",
                    })?;
                    let rp = load_gpr(&mut code, at_u32(view.extra, at + 1), R11)?;
                    let rv = load_gpr(&mut code, at_u32(view.extra, at + 2), R10)?;
                    let (rd, sp) = dst_gpr(inst)?;
                    if rd != rv {
                        x64::mov_rr(&mut code, true, rd, rv);
                    }
                    match kind {
                        AtomicRmw::Add => x64::lock_xadd64(&mut code, rp, rd),
                        AtomicRmw::Sub => {
                            x64::neg_r(&mut code, true, rd);
                            x64::lock_xadd64(&mut code, rp, rd);
                        }
                        AtomicRmw::Xchg => x64::xchg64(&mut code, rp, rd),
                        _ => {
                            return Err(TierError::Refused {
                                tier: TIER,
                                inst: Some(u32_of(inst)),
                                what: "unsupported x86-64 atomic_rmw",
                            });
                        }
                    }
                    if let Some(s) = sp {
                        x64::store_disp(&mut code, 8, rd, RSP, spill_off(spill_base, s));
                    }
                }
                Op::Fence => {
                    if !Flags(at_u8(view.flags, inst)).contains(Flags::SINGLE_THREAD)
                        && Flags(at_u8(view.flags, inst)).ordering() == Some(Ordering::SeqCst)
                    {
                        x64::mfence(&mut code);
                    }
                }
                Op::DataAddr => {
                    let (rd, sp) = dst_gpr(inst)?;
                    let rec = view.reference(RefId(at_u32(view.data, inst)));
                    match RefKind::from_word(rec.kind) {
                        Some(RefKind::Static) => {
                            let target = resolve_x64_outside(names, Outside::Static(rec.place), Space::Static, true, &mut out, inst)?;
                            let disp_at = u32_of(x64::mov_rip_got(&mut code, rd));
                            out.relocs.push(Reloc { at: disp_at, in_relro: false, kind: RelocKind::GotPc32, target, addend: -4 });
                        }
                        Some(RefKind::Vtable) => {
                            let target = resolve_x64_outside(names, Outside::Vtable(rec.place), Space::Vtable, false, &mut out, inst)?;
                            let disp_at = u32_of(x64::mov_rip_got(&mut code, rd));
                            out.relocs.push(Reloc { at: disp_at, in_relro: false, kind: RelocKind::GotPc32, target, addend: -4 });
                        }
                        Some(RefKind::Data) => {
                            let data = *view.datas.get(to_idx(rec.place)).ok_or(TierError::Refused {
                                tier: TIER,
                                inst: Some(u32_of(inst)),
                                what: "missing x64 data entry",
                            })?;
                            let align = 8usize.max(1 << data.align.min(12));
                            out.relro_align = out.relro_align.max(u8_of(data.align.min(12)));
                            while out.relro.len() % align != 0 {
                                out.relro.push(0);
                            }
                            let off = i64_of(out.relro.len());
                            let bytes = view.data_bytes_of(blaze_lir::func::DataId(rec.place));
                            if bytes.is_empty() {
                                out.relro.extend_from_slice(&[0; 8]);
                            } else {
                                out.relro.extend_from_slice(bytes);
                            }
                            let disp_at = u32_of(x64::lea_rip_rel(&mut code, rd));
                            out.relocs.push(Reloc { at: disp_at, in_relro: false, kind: RelocKind::Pc32, target: OWN_RELRO, addend: off - 4 });
                        }
                        _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unsupported x64 data_addr" }),
                    }
                    if let Some(s) = sp {
                        x64::store_disp(&mut code, 8, rd, RSP, spill_off(spill_base, s));
                    }
                }
                Op::FuncAddr => {
                    let (rd, sp) = dst_gpr(inst)?;
                    let rec = view.reference(RefId(at_u32(view.data, inst)));
                    let target = resolve_x64_outside(names, Outside::Func(rec.place), Space::Func, true, &mut out, inst)?;
                    let disp_at = u32_of(x64::mov_rip_got(&mut code, rd));
                    out.relocs.push(Reloc { at: disp_at, in_relro: false, kind: RelocKind::GotPc32, target, addend: -4 });
                    if let Some(s) = sp {
                        x64::store_disp(&mut code, 8, rd, RSP, spill_off(spill_base, s));
                    }
                }
                Op::Neg | Op::Not => {
                    let (rd, sp) = dst_gpr(inst)?;
                    let rs = load_gpr(&mut code, at_u32(view.data, inst), R10)?;
                    let w64 = bits != 32;
                    if rd != rs {
                        x64::mov_rr(&mut code, w64, rd, rs);
                    }
                    if op == Op::Neg {
                        x64::neg_r(&mut code, w64, rd);
                    } else {
                        x64::not_r(&mut code, w64, rd);
                    }
                    if bits < 32 {
                        narrow_x64(&mut code, rd, bits);
                    }
                    if let Some(s) = sp {
                        x64::store_disp(&mut code, 8, rd, RSP, spill_off(spill_base, s));
                    }
                }
                Op::Add => {
                    let (rd, sp) = dst_gpr(inst)?;
                    let w64 = bits != 32;
                    if let X64Fusion::LeaAddScale { a, b: sb, scale_log2, .. } = at_fusion(&fusion, inst) {
                        let ra = load_gpr(&mut code, a, R10)?;
                        let rb = load_gpr(&mut code, sb, R11)?;
                        x64::lea_sib(&mut code, w64, rd, ra, rb, scale_log2, 0);
                    } else {
                        let at = to_idx(at_u32(view.data, inst));
                        let (lhs, rhs) = (at_u32(view.extra, at), at_u32(view.extra, at + 1));
                        if let Some(imm) = at_opt(&const_u64, to_idx(rhs)).and_then(|v| i32::try_from(v.cast_signed()).ok()) {
                            let rl = load_gpr(&mut code, lhs, R10)?;
                            if rd == rl {
                                x64::alu_ri(&mut code, w64, 0, rd, imm);
                            } else {
                                x64::lea_base_disp(&mut code, w64, rd, rl, imm);
                            }
                        } else if let Some(imm) = at_opt(&const_u64, to_idx(lhs)).and_then(|v| i32::try_from(v.cast_signed()).ok()) {
                            let rr = load_gpr(&mut code, rhs, R10)?;
                            if rd == rr {
                                x64::alu_ri(&mut code, w64, 0, rd, imm);
                            } else {
                                x64::lea_base_disp(&mut code, w64, rd, rr, imm);
                            }
                        } else {
                            let rl = load_gpr(&mut code, lhs, R10)?;
                            let rr = load_gpr(&mut code, rhs, R11)?;
                            if rd != rl && rd != rr {
                                // 3-operand non-destructive `lea rd, [rl + rr]`!
                                x64::lea_sib(&mut code, w64, rd, rl, rr, 0, 0);
                            } else if rd == rl {
                                x64::alu_rr(&mut code, w64, 0x01, rd, rr);
                            } else {
                                x64::alu_rr(&mut code, w64, 0x01, rd, rl);
                            }
                        }
                        if bits < 32 {
                            narrow_x64(&mut code, rd, bits);
                        }
                    }
                    if let Some(s) = sp {
                        x64::store_disp(&mut code, 8, rd, RSP, spill_off(spill_base, s));
                    }
                }
                Op::Sub => {
                    let at = to_idx(at_u32(view.data, inst));
                    let (lhs, rhs) = (at_u32(view.extra, at), at_u32(view.extra, at + 1));
                    let (rd, sp) = dst_gpr(inst)?;
                    let w64 = bits != 32;
                    if let Some(imm) = at_opt(&const_u64, to_idx(rhs)).and_then(|v| i32::try_from(v.cast_signed()).ok()) {
                        let rl = load_gpr(&mut code, lhs, R10)?;
                        if rd == rl {
                            x64::alu_ri(&mut code, w64, 5, rd, imm);
                        } else if let Some(neg_imm) = imm.checked_neg() {
                            x64::lea_base_disp(&mut code, w64, rd, rl, neg_imm);
                        } else {
                            x64::mov_rr(&mut code, w64, rd, rl);
                            x64::alu_ri(&mut code, w64, 5, rd, imm);
                        }
                    } else {
                        let rl = load_gpr(&mut code, lhs, R10)?;
                        let rr = load_gpr(&mut code, rhs, R11)?;
                        if rd == rr && rd != rl {
                            x64::mov_rr(&mut code, w64, R11, rr);
                            x64::mov_rr(&mut code, w64, rd, rl);
                            x64::alu_rr(&mut code, w64, 0x29, rd, R11);
                        } else {
                            if rd != rl {
                                x64::mov_rr(&mut code, w64, rd, rl);
                            }
                            x64::alu_rr(&mut code, w64, 0x29, rd, rr);
                        }
                    }
                    if bits < 32 {
                        narrow_x64(&mut code, rd, bits);
                    }
                    if let Some(s) = sp {
                        x64::store_disp(&mut code, 8, rd, RSP, spill_off(spill_base, s));
                    }
                }
                Op::Mul => {
                    let at = to_idx(at_u32(view.data, inst));
                    let (lhs, rhs) = (at_u32(view.extra, at), at_u32(view.extra, at + 1));
                    let (rd, sp) = dst_gpr(inst)?;
                    let w64 = bits != 32;
                    let imm_pair = if let Some(c) = at_opt(&const_u64, to_idx(rhs)).and_then(|v| i32::try_from(v.cast_signed()).ok()) {
                        Some((lhs, c))
                    } else if let Some(c) = at_opt(&const_u64, to_idx(lhs)).and_then(|v| i32::try_from(v.cast_signed()).ok()) {
                        Some((rhs, c))
                    } else {
                        None
                    };
                    if let Some((x, c)) = imm_pair {
                        let rx = load_gpr(&mut code, x, R10)?;
                        // 1-cycle `lea` for `*3`, `*5`, `*9`!
                        match c {
                            3 => x64::lea_sib(&mut code, w64, rd, rx, rx, 1, 0),
                            5 => x64::lea_sib(&mut code, w64, rd, rx, rx, 2, 0),
                            9 => x64::lea_sib(&mut code, w64, rd, rx, rx, 3, 0),
                            _ => x64::imul_rri(&mut code, w64, rd, rx, c),
                        }
                    } else {
                        let rl = load_gpr(&mut code, lhs, R10)?;
                        let rr = load_gpr(&mut code, rhs, R11)?;
                        if rd == rr && rd != rl {
                            x64::imul_rr(&mut code, w64, rd, rl);
                        } else {
                            if rd != rl {
                                x64::mov_rr(&mut code, w64, rd, rl);
                            }
                            x64::imul_rr(&mut code, w64, rd, rr);
                        }
                    }
                    if bits < 32 {
                        narrow_x64(&mut code, rd, bits);
                    }
                    if let Some(s) = sp {
                        x64::store_disp(&mut code, 8, rd, RSP, spill_off(spill_base, s));
                    }
                }
                Op::And | Op::Or | Op::Xor => {
                    let at = to_idx(at_u32(view.data, inst));
                    let (lhs, rhs) = (at_u32(view.extra, at), at_u32(view.extra, at + 1));
                    let (rd, sp) = dst_gpr(inst)?;
                    let w64 = bits != 32;
                    let (rr_op, ri_ext) = match op {
                        Op::And => (0x21u8, 4u8),
                        Op::Or => (0x09, 1),
                        _ => (0x31, 6),
                    };
                    let imm_pair = if let Some(c) = at_opt(&const_u64, to_idx(rhs)).and_then(|v| i32::try_from(v.cast_signed()).ok()) {
                        Some((lhs, c))
                    } else if let Some(c) = at_opt(&const_u64, to_idx(lhs)).and_then(|v| i32::try_from(v.cast_signed()).ok()) {
                        Some((rhs, c))
                    } else {
                        None
                    };
                    if let Some((x, c)) = imm_pair {
                        let rx = load_gpr(&mut code, x, R10)?;
                        if rd != rx {
                            x64::mov_rr(&mut code, w64, rd, rx);
                        }
                        x64::alu_ri(&mut code, w64, ri_ext, rd, c);
                    } else {
                        let rl = load_gpr(&mut code, lhs, R10)?;
                        let rr = load_gpr(&mut code, rhs, R11)?;
                        if rd == rr && rd != rl {
                            x64::alu_rr(&mut code, w64, rr_op, rd, rl);
                        } else {
                            if rd != rl {
                                x64::mov_rr(&mut code, w64, rd, rl);
                            }
                            x64::alu_rr(&mut code, w64, rr_op, rd, rr);
                        }
                    }
                    if let Some(s) = sp {
                        x64::store_disp(&mut code, 8, rd, RSP, spill_off(spill_base, s));
                    }
                }
                Op::Shl | Op::Lshr | Op::Ashr | Op::Rotl | Op::Rotr => {
                    let at = to_idx(at_u32(view.data, inst));
                    let (lhs, rhs) = (at_u32(view.extra, at), at_u32(view.extra, at + 1));
                    let (rd, sp) = dst_gpr(inst)?;
                    let w64 = bits != 32;
                    let ext = match op {
                        Op::Rotl => 0u8,
                        Op::Rotr => 1,
                        Op::Shl => 4,
                        Op::Lshr => 5,
                        _ => 7,
                    };
                    let rl = load_gpr(&mut code, lhs, R10)?;
                    if let Some(k) = at_opt(&const_u64, to_idx(rhs)) {
                        let sh = u8_of(k & u64::from(bits - 1));
                        if rd != rl {
                            x64::mov_rr(&mut code, w64, rd, rl);
                        }
                        if sh > 0 {
                            x64::shift_ri(&mut code, w64, ext, rd, sh);
                        }
                    } else if has_bmi2 && matches!(op, Op::Shl | Op::Lshr | Op::Ashr) && (bits == 64 || bits == 32) {
                        // BMI2 3-operand variable shift (`shlx`/`shrx`/`sarx`) — ZERO `rcx` clobber!
                        let rr = load_gpr(&mut code, rhs, R11)?;
                        let pp = match op {
                            Op::Shl => 1u8,
                            Op::Ashr => 2,
                            _ => 3, // Op::Lshr
                        };
                        x64::bmi2_shiftx(&mut code, w64, pp, rd, rl, rr);
                    } else {
                        // Variable shift uses `CL`: save `RCX` into `R11` if needed.
                        let rr = load_gpr(&mut code, rhs, R11)?;
                        let save_rcx = rr != RCX && rd != RCX;
                        if save_rcx {
                            x64::push_r64(&mut code, RCX);
                        }
                        if rd == RCX {
                            x64::mov_rr(&mut code, true, R10, rl);
                            x64::mov_rr(&mut code, true, RCX, rr);
                            x64::shift_cl(&mut code, w64, ext, R10);
                            x64::mov_rr(&mut code, true, rd, R10);
                        } else {
                            if rd != rl {
                                x64::mov_rr(&mut code, w64, rd, rl);
                            }
                            if rr != RCX {
                                x64::mov_rr(&mut code, true, RCX, rr);
                            }
                            x64::shift_cl(&mut code, w64, ext, rd);
                        }
                        if save_rcx {
                            x64::pop_r64(&mut code, RCX);
                        }
                    }
                    if bits < 32 {
                        narrow_x64(&mut code, rd, bits);
                    }
                    if let Some(s) = sp {
                        x64::store_disp(&mut code, 8, rd, RSP, spill_off(spill_base, s));
                    }
                }
                Op::Udiv | Op::Urem | Op::Sdiv | Op::Srem | Op::Umulhi | Op::Smulhi => {
                    let at = to_idx(at_u32(view.data, inst));
                    let ra_v = at_u32(view.extra, at);
                    let rb_v = at_u32(view.extra, at + 1);
                    let (rd, sp) = dst_gpr(inst)?;
                    // BMI2 `mulx` fast path for `Umulhi` and Granlund–Montgomery magic `udiv`:
                    // saves ZERO `RAX` and touches ZERO `RFLAGS`!
                    if has_bmi2
                        && bits == 64
                        && (op == Op::Umulhi
                            || (op == Op::Udiv && at_opt(&const_u64, to_idx(rb_v)).and_then(compute_magic_udiv64).is_some()))
                    {
                        let save_rdx = rd != RDX;
                        if save_rdx {
                            x64::push_r64(&mut code, RDX);
                        }
                        let sp_adj = if save_rdx { 8i32 } else { 0i32 };
                        let load_adj_bmi2 = |buf: &mut Vec<u8, Alloc<'a>>, v: u32, scratch: XReg| -> Result<XReg, TierError> {
                            match at_loc(&loc_of, to_idx(v)) {
                                X64Loc::Gpr(r) => Ok(r),
                                X64Loc::GprSpill(s) => {
                                    x64::load_disp(buf, 8, scratch, RSP, spill_off(spill_base.wrapping_add(sp_adj), s));
                                    Ok(scratch)
                                }
                                X64Loc::None => {
                                    x64::mov_imm64(buf, scratch, at_opt(&const_u64, to_idx(v)).unwrap_or(0));
                                    Ok(scratch)
                                }
                                _ => Err(TierError::Refused { tier: TIER, inst: Some(v), what: "expected x64 GPR" }),
                            }
                        };
                        let mut ra = load_adj_bmi2(&mut code, ra_v, R10)?;
                        if op == Op::Umulhi {
                            let rb = load_adj_bmi2(&mut code, rb_v, R11)?;
                            if ra == RDX {
                                x64::bmi2_mulx(&mut code, R10, R11, rb);
                            } else if rb == RDX {
                                x64::bmi2_mulx(&mut code, R10, R11, ra);
                            } else {
                                x64::mov_rr(&mut code, true, RDX, ra);
                                x64::bmi2_mulx(&mut code, R10, R11, rb);
                            }
                        } else {
                            let plan = at_opt(&const_u64, to_idx(rb_v))
                                .and_then(compute_magic_udiv64)
                                .ok_or(TierError::Refused {
                                    tier: TIER,
                                    inst: Some(u32_of(inst)),
                                    what: "missing magic udiv64 plan",
                                })?;
                            if ra == RDX {
                                x64::mov_rr(&mut code, true, R10, RDX);
                                ra = R10;
                            }
                            match plan {
                                MagicUdiv64::CompareGe(d) => {
                                    x64::mov_imm64(&mut code, R11, d);
                                    x64::alu_rr(&mut code, true, 0x39, ra, R11);
                                    x64::setcc_zext(&mut code, Cc::Ae, R10);
                                }
                                MagicUdiv64::MulShift { magic, shift } => {
                                    x64::mov_imm64(&mut code, RDX, magic);
                                    x64::bmi2_mulx(&mut code, R10, R11, ra); // R10 = umulh(magic, ra)
                                    if shift > 0 {
                                        x64::shift_ri(&mut code, true, 5, R10, u8_of(shift));
                                    }
                                }
                                MagicUdiv64::MulAddShift { magic, post_shift } => {
                                    if ra != R10 {
                                        x64::mov_rr(&mut code, true, R10, ra);
                                    }
                                    x64::mov_imm64(&mut code, RDX, magic);
                                    x64::bmi2_mulx(&mut code, RDX, R11, R10); // RDX = q
                                    x64::alu_rr(&mut code, true, 0x29, R10, RDX); // R10 = ra - q
                                    x64::shift_ri(&mut code, true, 5, R10, 1); // >> 1
                                    x64::alu_rr(&mut code, true, 0x01, R10, RDX); // + q
                                    if post_shift > 0 {
                                        x64::shift_ri(&mut code, true, 5, R10, u8_of(post_shift));
                                    }
                                }
                            }
                        }
                        if save_rdx {
                            x64::pop_r64(&mut code, RDX);
                        }
                        if rd != R10 {
                            x64::mov_rr(&mut code, true, rd, R10);
                        }
                        if let Some(s) = sp {
                            x64::store_disp(&mut code, 8, rd, RSP, spill_off(spill_base, s));
                        }
                        inst += 1;
                        continue;
                    }
                    // Preserve `RAX` / `RDX` across fixed-register `mul`/`div`/`idiv` instructions!
                    let save_rax = rd != RAX;
                    let save_rdx = rd != RDX;
                    if save_rax {
                        x64::push_r64(&mut code, RAX);
                    }
                    if save_rdx {
                        x64::push_r64(&mut code, RDX);
                    }
                    // Note: stack pointer shifted by `(save_rax + save_rdx) * 8`!
                    let sp_adj = (if save_rax { 8i32 } else { 0i32 }) + (if save_rdx { 8i32 } else { 0i32 });
                    let load_adj = |buf: &mut Vec<u8, Alloc<'a>>, v: u32, scratch: XReg| -> Result<XReg, TierError> {
                        match at_loc(&loc_of, to_idx(v)) {
                            X64Loc::Gpr(r) => Ok(r),
                            X64Loc::GprSpill(s) => {
                                x64::load_disp(buf, 8, scratch, RSP, spill_off(spill_base.wrapping_add(sp_adj), s));
                                Ok(scratch)
                            }
                            X64Loc::None => {
                                x64::mov_imm64(buf, scratch, at_opt(&const_u64, to_idx(v)).unwrap_or(0));
                                Ok(scratch)
                            }
                            _ => Err(TierError::Refused { tier: TIER, inst: Some(v), what: "expected x64 GPR" }),
                        }
                    };
                    let ra = load_adj(&mut code, ra_v, R10)?;
                    if op == Op::Udiv && bits == 64 {
                        if let Some(plan) = at_opt(&const_u64, to_idx(rb_v)).and_then(compute_magic_udiv64) {
                            match plan {
                                MagicUdiv64::CompareGe(d) => {
                                    x64::mov_imm64(&mut code, R11, d);
                                    x64::alu_rr(&mut code, true, 0x39, ra, R11);
                                    x64::setcc_zext(&mut code, Cc::Ae, R10);
                                }
                                MagicUdiv64::MulShift { magic, shift } => {
                                    x64::mov_rr(&mut code, true, RAX, ra);
                                    x64::mov_imm64(&mut code, R11, magic);
                                    x64::mul_r64(&mut code, R11); // RDX = umulh(ra, magic)
                                    if shift > 0 {
                                        x64::shift_ri(&mut code, true, 5, RDX, u8_of(shift));
                                    }
                                    x64::mov_rr(&mut code, true, R10, RDX);
                                }
                                MagicUdiv64::MulAddShift { magic, post_shift } => {
                                    x64::mov_rr(&mut code, true, R10, ra);
                                    x64::mov_rr(&mut code, true, RAX, ra);
                                    x64::mov_imm64(&mut code, R11, magic);
                                    x64::mul_r64(&mut code, R11); // RDX = q
                                    x64::alu_rr(&mut code, true, 0x29, R10, RDX); // R10 = ra - q
                                    x64::shift_ri(&mut code, true, 5, R10, 1); // >> 1
                                    x64::alu_rr(&mut code, true, 0x01, R10, RDX); // + q
                                    if post_shift > 0 {
                                        x64::shift_ri(&mut code, true, 5, R10, u8_of(post_shift));
                                    }
                                }
                            }
                            if save_rdx {
                                x64::pop_r64(&mut code, RDX);
                            }
                            if save_rax {
                                x64::pop_r64(&mut code, RAX);
                            }
                            if rd != R10 {
                                x64::mov_rr(&mut code, true, rd, R10);
                            }
                            if let Some(s) = sp {
                                x64::store_disp(&mut code, 8, rd, RSP, spill_off(spill_base, s));
                            }
                            inst += 1;
                            continue;
                        }
                    }
                    let rb = load_adj(&mut code, rb_v, R11)?;
                    x64::mov_rr(&mut code, true, R11, rb);
                    x64::mov_rr(&mut code, true, RAX, ra);
                    match op {
                        Op::Umulhi => {
                            x64::mul_r64(&mut code, R11);
                            x64::mov_rr(&mut code, true, R10, RDX);
                        }
                        Op::Smulhi => {
                            x64::imul_1op_r64(&mut code, R11);
                            x64::mov_rr(&mut code, true, R10, RDX);
                        }
                        Op::Udiv | Op::Urem => {
                            x64::mov_imm64(&mut code, RDX, 0);
                            x64::div_r64(&mut code, R11);
                            x64::mov_rr(&mut code, true, R10, if op == Op::Udiv { RAX } else { RDX });
                        }
                        Op::Sdiv | Op::Srem => {
                            if bits < 64 {
                                match bits {
                                    8 => {
                                        x64::movsx_b(&mut code, RAX, RAX);
                                        x64::movsx_b(&mut code, R11, R11);
                                    }
                                    16 => {
                                        x64::movsx_w(&mut code, RAX, RAX);
                                        x64::movsx_w(&mut code, R11, R11);
                                    }
                                    32 => {
                                        x64::movsxd(&mut code, RAX, RAX);
                                        x64::movsxd(&mut code, R11, R11);
                                    }
                                    _ => {}
                                }
                            }
                            x64::cqo(&mut code);
                            x64::idiv_r64(&mut code, R11);
                            x64::mov_rr(&mut code, true, R10, if op == Op::Sdiv { RAX } else { RDX });
                        }
                        _ => {
                            return Err(TierError::Refused {
                                tier: TIER,
                                inst: Some(u32_of(inst)),
                                what: "unsupported x64 div/mulhi op",
                            });
                        }
                    }
                    if save_rdx {
                        x64::pop_r64(&mut code, RDX);
                    }
                    if save_rax {
                        x64::pop_r64(&mut code, RAX);
                    }
                    if rd != R10 {
                        x64::mov_rr(&mut code, true, rd, R10);
                    }
                    if bits < 64 {
                        narrow_x64(&mut code, rd, bits);
                    }
                    if let Some(s) = sp {
                        x64::store_disp(&mut code, 8, rd, RSP, spill_off(spill_base, s));
                    }
                }
                Op::Umin | Op::Umax | Op::Smin | Op::Smax => {
                    let at = to_idx(at_u32(view.data, inst));
                    let (rd, sp) = dst_gpr(inst)?;
                    let rl = load_gpr(&mut code, at_u32(view.extra, at), R10)?;
                    let rr = load_gpr(&mut code, at_u32(view.extra, at + 1), R11)?;
                    let cc = match op {
                        Op::Umin => Cc::A,
                        Op::Umax => Cc::B,
                        Op::Smin => Cc::G,
                        _ => Cc::L,
                    };
                    let w64 = bits != 32;
                    x64::alu_rr(&mut code, w64, 0x39, rl, rr);
                    if rd != rl {
                        x64::mov_rr(&mut code, w64, rd, rl);
                    }
                    x64::cmovcc(&mut code, w64, cc, rd, rr);
                    if let Some(s) = sp {
                        x64::store_disp(&mut code, 8, rd, RSP, spill_off(spill_base, s));
                    }
                }
                Op::Clz | Op::Ctz | Op::Popcnt | Op::Bswap => {
                    let (rd, sp) = dst_gpr(inst)?;
                    let rs = load_gpr(&mut code, at_u32(view.data, inst), R10)?;
                    let w64 = bits != 32;
                    match op {
                        Op::Clz => {
                            x64::lzcnt(&mut code, w64, rd, rs);
                            if bits < 32 {
                                x64::alu_ri(&mut code, true, 5, rd, i32_of(64 - bits));
                            }
                        }
                        Op::Ctz => x64::tzcnt(&mut code, w64, rd, rs),
                        Op::Popcnt => x64::popcnt(&mut code, w64, rd, rs),
                        Op::Bswap => {
                            if rd != rs {
                                x64::mov_rr(&mut code, w64, rd, rs);
                            }
                            if bits == 64 || bits == 32 {
                                x64::bswap(&mut code, bits == 64, rd);
                            } else if bits == 16 {
                                x64::shift_ri(&mut code, false, 1, rd, 8);
                                x64::movzx_w(&mut code, rd, rd);
                            }
                        }
                        _ => {
                            return Err(TierError::Refused {
                                tier: TIER,
                                inst: Some(u32_of(inst)),
                                what: "unsupported x64 bitop",
                            });
                        }
                    }
                    if let Some(s) = sp {
                        x64::store_disp(&mut code, 8, rd, RSP, spill_off(spill_base, s));
                    }
                }
                Op::Icmp => {
                    let at = to_idx(at_u32(view.data, inst));
                    let cond = IntCond::from_word(at_u32(view.extra, at)).ok_or(TierError::Refused {
                        tier: TIER,
                        inst: Some(u32_of(inst)),
                        what: "invalid x86-64 icmp cond",
                    })?;
                    let (a, bv) = (at_u32(view.extra, at + 1), at_u32(view.extra, at + 2));
                    let (rd, sp) = dst_gpr(inst)?;
                    emit_cmp_x64(&mut code, cond, a, bv, ty_at(view.tys, to_idx(a)))?;
                    x64::setcc_zext(&mut code, cc_of(cond), rd);
                    if let Some(s) = sp {
                        x64::store_disp(&mut code, 8, rd, RSP, spill_off(spill_base, s));
                    }
                }
                Op::Fcmp => {
                    let at = to_idx(at_u32(view.data, inst));
                    let cond = FloatCond::from_word(at_u32(view.extra, at)).ok_or(TierError::Refused {
                        tier: TIER,
                        inst: Some(u32_of(inst)),
                        what: "invalid x86-64 fcmp cond",
                    })?;
                    let (a, bv) = (at_u32(view.extra, at + 1), at_u32(view.extra, at + 2));
                    let double = is_fp_ty(ty_at(view.tys, to_idx(a))).ok_or(TierError::Refused {
                        tier: TIER,
                        inst: Some(u32_of(inst)),
                        what: "non-fp x86-64 fcmp",
                    })?;
                    let (rd, sp) = dst_gpr(inst)?;
                    match cond {
                        FloatCond::False => x64::mov_imm64(&mut code, rd, 0),
                        FloatCond::True => x64::mov_imm64(&mut code, rd, 1),
                        _ => {
                            let va = load_xmm(&mut code, a, double, XMM14)?;
                            let vb = load_xmm(&mut code, bv, double, XMM15)?;
                            // `ucomisd`/`ucomiss` sets ZF=PF=CF=1 on NaN; swap operands for `<`/`<=`
                            // so `A` (`CF=0 && ZF=0`) and `Ae` (`CF=0`) automatically reject NaN!
                            match cond {
                                FloatCond::Ogt => {
                                    x64::ucomis(&mut code, double, va, vb);
                                    x64::setcc_zext(&mut code, Cc::A, rd);
                                }
                                FloatCond::Oge => {
                                    x64::ucomis(&mut code, double, va, vb);
                                    x64::setcc_zext(&mut code, Cc::Ae, rd);
                                }
                                FloatCond::Olt => {
                                    x64::ucomis(&mut code, double, vb, va);
                                    x64::setcc_zext(&mut code, Cc::A, rd);
                                }
                                FloatCond::Ole => {
                                    x64::ucomis(&mut code, double, vb, va);
                                    x64::setcc_zext(&mut code, Cc::Ae, rd);
                                }
                                FloatCond::Oeq => {
                                    x64::ucomis(&mut code, double, va, vb);
                                    x64::setcc_zext(&mut code, Cc::E, rd);
                                    x64::setcc_zext(&mut code, Cc::Np, R11);
                                    x64::alu_rr(&mut code, false, 0x21, rd, R11);
                                }
                                FloatCond::One => {
                                    x64::ucomis(&mut code, double, va, vb);
                                    x64::setcc_zext(&mut code, Cc::Ne, rd);
                                    x64::setcc_zext(&mut code, Cc::Np, R11);
                                    x64::alu_rr(&mut code, false, 0x21, rd, R11);
                                }
                                FloatCond::Ord => {
                                    x64::ucomis(&mut code, double, va, vb);
                                    x64::setcc_zext(&mut code, Cc::Np, rd);
                                }
                                FloatCond::Uno => {
                                    x64::ucomis(&mut code, double, va, vb);
                                    x64::setcc_zext(&mut code, Cc::P, rd);
                                }
                                FloatCond::Ueq => {
                                    x64::ucomis(&mut code, double, va, vb);
                                    x64::setcc_zext(&mut code, Cc::E, rd);
                                }
                                FloatCond::Une => {
                                    x64::ucomis(&mut code, double, va, vb);
                                    x64::setcc_zext(&mut code, Cc::Ne, rd);
                                    x64::setcc_zext(&mut code, Cc::P, R11);
                                    x64::alu_rr(&mut code, false, 0x09, rd, R11);
                                }
                                FloatCond::Ult => {
                                    x64::ucomis(&mut code, double, va, vb);
                                    x64::setcc_zext(&mut code, Cc::B, rd);
                                }
                                FloatCond::Ule => {
                                    x64::ucomis(&mut code, double, va, vb);
                                    x64::setcc_zext(&mut code, Cc::Be, rd);
                                }
                                FloatCond::Ugt => {
                                    x64::ucomis(&mut code, double, vb, va);
                                    x64::setcc_zext(&mut code, Cc::B, rd);
                                }
                                FloatCond::Uge => {
                                    x64::ucomis(&mut code, double, vb, va);
                                    x64::setcc_zext(&mut code, Cc::Be, rd);
                                }
                                FloatCond::False | FloatCond::True => {}
                            }
                        }
                    }
                    if let Some(s) = sp {
                        x64::store_disp(&mut code, 8, rd, RSP, spill_off(spill_base, s));
                    }
                }
                Op::Select => {
                    let at = to_idx(at_u32(view.data, inst));
                    let (t, f) = (at_u32(view.extra, at + 1), at_u32(view.extra, at + 2));
                    let (rd, sp) = dst_gpr(inst)?;
                    let cc = match at_fusion(&fusion, inst) {
                        X64Fusion::CmpSelect { cond, a, b: bv, op_ty } => {
                            emit_cmp_x64(&mut code, cond, a, bv, op_ty)?;
                            cc_of(cond)
                        }
                        _ => {
                            let rc = load_gpr(&mut code, at_u32(view.extra, at), R10)?;
                            x64::test_rr(&mut code, false, rc, rc);
                            Cc::Ne
                        }
                    };
                    let rt = load_gpr(&mut code, t, R10)?;
                    let rf = load_gpr(&mut code, f, R11)?;
                    // `rd = if cc { rt } else { rf }` -> `mov rd, rf; cmovcc rd, rt`!
                    if rd == rt && rd != rf {
                        x64::cmovcc(&mut code, true, cc.invert(), rd, rf);
                    } else {
                        if rd != rf {
                            x64::mov_rr(&mut code, true, rd, rf);
                        }
                        x64::cmovcc(&mut code, true, cc, rd, rt);
                    }
                    if let Some(s) = sp {
                        x64::store_disp(&mut code, 8, rd, RSP, spill_off(spill_base, s));
                    }
                }
                Op::Zext | Op::Trunc => {
                    let (rd, sp) = dst_gpr(inst)?;
                    let rs = load_gpr(&mut code, at_u32(view.data, inst), R10)?;
                    if rd != rs {
                        x64::mov_rr(&mut code, true, rd, rs);
                    }
                    narrow_x64(&mut code, rd, bits);
                    if let Some(s) = sp {
                        x64::store_disp(&mut code, 8, rd, RSP, spill_off(spill_base, s));
                    }
                }
                Op::Sext => {
                    let src_v = at_u32(view.data, inst);
                    let src_bits = ty_at(view.tys, to_idx(src_v)).bits(64);
                    let (rd, sp) = dst_gpr(inst)?;
                    let rs = load_gpr(&mut code, src_v, R10)?;
                    match src_bits {
                        8 => x64::movsx_b(&mut code, rd, rs),
                        16 => x64::movsx_w(&mut code, rd, rs),
                        32 => x64::movsxd(&mut code, rd, rs),
                        _ => {
                            if rd != rs {
                                x64::mov_rr(&mut code, true, rd, rs);
                            }
                        }
                    }
                    if bits < 64 {
                        narrow_x64(&mut code, rd, bits);
                    }
                    if let Some(s) = sp {
                        x64::store_disp(&mut code, 8, rd, RSP, spill_off(spill_base, s));
                    }
                }
                Op::Fadd | Op::Fsub | Op::Fmul | Op::Fdiv | Op::Fma | Op::Fmuladd => {
                    let double = fp_ty.ok_or(TierError::Refused {
                        tier: TIER,
                        inst: Some(u32_of(inst)),
                        what: "non-fp x86-64 fp op",
                    })?;
                    let (vd, sp) = dst_xmm(inst)?;
                    let at = to_idx(at_u32(view.data, inst));
                    let fma_ops = match (op, at_fusion(&fusion, inst)) {
                        (Op::Fma | Op::Fmuladd, _) => {
                            Some((at_u32(view.extra, at), at_u32(view.extra, at + 1), at_u32(view.extra, at + 2)))
                        }
                        (Op::Fadd, X64Fusion::Fma { a, b, acc }) => Some((a, b, acc)),
                        _ => None,
                    };
                    if let Some((a_v, b_v, c_v)) = fma_ops {
                        let loc_a = at_loc(&loc_of, to_idx(a_v));
                        let loc_b = at_loc(&loc_of, to_idx(b_v));
                        let loc_c = at_loc(&loc_of, to_idx(c_v));
                        if let (X64Loc::XmmSpill(sb), true) = (loc_b, sp.is_some() && !matches!(loc_a, X64Loc::Xmm(_)) && !matches!(loc_c, X64Loc::Xmm(_))) {
                            let vc = load_xmm(&mut code, c_v, double, XMM14)?;
                            if vc != vd {
                                x64::movfp_rr(&mut code, double, vd, vc);
                            }
                            let va = load_xmm(&mut code, a_v, double, XMM15)?;
                            x64::fma3_vfmadd231_mem(&mut code, double, false, vd, va, RSP, spill_off(spill_base, sb));
                        } else {
                            let va = load_xmm(&mut code, a_v, double, XMM14)?;
                            let vb = load_xmm(&mut code, b_v, double, XMM15)?;
                            let vc = match loc_c {
                                X64Loc::Xmm(r) => r,
                                _ => {
                                    let dst_c = if vd != va && vd != vb { vd } else if va != XMM14 && vb != XMM14 { XMM14 } else { XMM15 };
                                    load_xmm(&mut code, c_v, double, dst_c)?
                                }
                            };
                            if vd == vc {
                                x64::fma3_vfmadd231(&mut code, double, false, vd, va, vb);
                            } else if vd == va {
                                x64::fma3_vfmadd213(&mut code, double, false, vd, vb, vc);
                            } else if vd == vb {
                                x64::fma3_vfmadd213(&mut code, double, false, vd, va, vc);
                            } else {
                                x64::movfp_rr(&mut code, double, vd, vc);
                                x64::fma3_vfmadd231(&mut code, double, false, vd, va, vb);
                            }
                        }
                    } else {
                        let va = load_xmm(&mut code, at_u32(view.extra, at), double, XMM14)?;
                        let vb = load_xmm(&mut code, at_u32(view.extra, at + 1), double, XMM15)?;
                        let opc = match op {
                            Op::Fadd => 0x58u8,
                            Op::Fmul => 0x59,
                            Op::Fsub => 0x5c,
                            _ => 0x5e,
                        };
                        if vd == vb && vd != va {
                            x64::movfp_rr(&mut code, double, XMM15, vb);
                            x64::movfp_rr(&mut code, double, vd, va);
                            x64::fp_bin(&mut code, double, opc, vd, XMM15);
                        } else {
                            if vd != va {
                                x64::movfp_rr(&mut code, double, vd, va);
                            }
                            x64::fp_bin(&mut code, double, opc, vd, vb);
                        }
                    }
                    if let Some(s) = sp {
                        x64::store_fp_disp(&mut code, double, vd, RSP, spill_off(spill_base, s));
                    }
                }
                Op::Bitcast => {
                    let src_v = at_u32(view.data, inst);
                    let src_fp = is_fp_ty(ty_at(view.tys, to_idx(src_v)));
                    match (src_fp, fp_ty) {
                        (Some(d), None) => {
                            let (rd, sp) = dst_gpr(inst)?;
                            let vs = load_xmm(&mut code, src_v, d, XMM14)?;
                            x64::mov_xmm_to_gpr(&mut code, d, rd, vs);
                            if let Some(s) = sp {
                                x64::store_disp(&mut code, 8, rd, RSP, spill_off(spill_base, s));
                            }
                        }
                        (None, Some(d)) => {
                            let (vd, sp) = dst_xmm(inst)?;
                            let rs = load_gpr(&mut code, src_v, R10)?;
                            x64::mov_gpr_to_xmm(&mut code, d, vd, rs);
                            if let Some(s) = sp {
                                x64::store_fp_disp(&mut code, d, vd, RSP, spill_off(spill_base, s));
                            }
                        }
                        _ => {
                            let (rd, sp) = dst_gpr(inst)?;
                            let rs = load_gpr(&mut code, src_v, R10)?;
                            if rd != rs {
                                x64::mov_rr(&mut code, true, rd, rs);
                            }
                            if let Some(s) = sp {
                                x64::store_disp(&mut code, 8, rd, RSP, spill_off(spill_base, s));
                            }
                        }
                    }
                }
                Op::Call | Op::CallIndirect => {
                    let at = to_idx(at_u32(view.data, inst));
                    let sig_id = SigId(at_u32(view.extra, at + 1));
                    let argc = to_idx(at_u32(view.extra, at + 2));
                    let is_tail = at_bool(&tail_calls, inst);
                    let direct_target = if op == Op::Call {
                        let rec = view.reference(RefId(at_u32(view.extra, at)));
                        Some(resolve_x64_outside(names, Outside::Func(rec.place), Space::Func, true, &mut out, inst)?)
                    } else {
                        None
                    };
                    gpr_moves.clear();
                    xmm_moves.clear();
                    const_writes.clear();
                    if op == Op::CallIndirect {
                        let cv = at_u32(view.extra, at);
                        gpr_moves.push((X64Loc::Gpr(R11), at_loc(&loc_of, to_idx(cv))));
                    }
                    let (mut gk, mut fk) = (0usize, 0usize);
                    for k in 0..argc {
                        let av = at_u32(view.extra, at + 3 + k);
                        let avi = to_idx(av);
                        if is_fp_ty(ty_at(view.tys, avi)).is_some() {
                            let &reg = SYSV_ARG_FP.get(fk).ok_or(TierError::Refused {
                                tier: TIER,
                                inst: Some(u32_of(inst)),
                                what: "x64 FP call arg overflow",
                            })?;
                            xmm_moves.push((X64Loc::Xmm(reg), at_loc(&loc_of, avi)));
                            fk += 1;
                        } else {
                            let &reg = SYSV_ARG_GPR.get(gk).ok_or(TierError::Refused {
                                tier: TIER,
                                inst: Some(u32_of(inst)),
                                what: "x64 GPR call arg overflow",
                            })?;
                            let dst = X64Loc::Gpr(reg);
                            gk += 1;
                            let src = at_loc(&loc_of, avi);
                            if src == X64Loc::None {
                                const_writes.push((dst, at_opt(&const_u64, avi).unwrap_or(0)));
                            } else {
                                gpr_moves.push((dst, src));
                            }
                        }
                    }
                    emit_parallel_x64_gpr(&mut code, &mut gpr_moves, spill_base);
                    emit_parallel_x64_xmm(&mut code, &mut xmm_moves, spill_base, has_ymm);
                    for &(dst, val) in const_writes.iter() {
                        if let X64Loc::Gpr(r) = dst {
                            x64::mov_imm64(&mut code, r, val);
                        }
                    }
                    if is_tail {
                        emit_epilogue(&mut code);
                        if let Some(target) = direct_target {
                            let disp_at = u32_of(x64::jmp_rel32(&mut code));
                            out.relocs.push(Reloc { at: disp_at, in_relro: false, kind: RelocKind::Pc32, target, addend: -4 });
                        } else {
                            x64::jmp_r64(&mut code, R11);
                        }
                        inst += 2;
                        continue;
                    }
                    if let Some(target) = direct_target {
                        match call_mode {
                            CallMode::DirectBl => {
                                let disp_at = u32_of(x64::call_rel32(&mut code));
                                out.relocs.push(Reloc { at: disp_at, in_relro: false, kind: RelocKind::Pc32, target, addend: -4 });
                            }
                            CallMode::GotSlot => {
                                let disp_at = u32_of(x64::call_rip_got(&mut code));
                                out.relocs.push(Reloc { at: disp_at, in_relro: false, kind: RelocKind::GotPc32, target, addend: -4 });
                            }
                        }
                    } else {
                        x64::call_r64(&mut code, R11);
                    }
                    if loc != X64Loc::None && ty != Ty::NONE {
                        if let Some(double) = fp_ty {
                            match loc {
                                X64Loc::Xmm(d) if d != 0 => x64::movfp_rr(&mut code, double, d, 0),
                                X64Loc::XmmSpill(s) => x64::store_fp_disp(&mut code, double, 0, RSP, spill_off(spill_base, s)),
                                _ => {}
                            }
                        } else {
                            match loc {
                                X64Loc::Gpr(d) => {
                                    if d != RAX {
                                        x64::mov_rr(&mut code, true, d, RAX);
                                    }
                                    if bits < 64 {
                                        narrow_x64(&mut code, d, bits);
                                    }
                                }
                                X64Loc::GprSpill(s) => {
                                    if bits < 64 {
                                        narrow_x64(&mut code, RAX, bits);
                                    }
                                    x64::store_disp(&mut code, 8, RAX, RSP, spill_off(spill_base, s));
                                }
                                _ => {}
                            }
                        }
                    }
                    let _ = sig_id;
                }
                Op::Jump => {
                    let at = to_idx(at_u32(view.data, inst));
                    let tb = at_u32(view.extra, at);
                    let argc = to_idx(at_u32(view.extra, at + 1));
                    emit_edge_x64(&mut code, &mut gpr_moves, &mut xmm_moves, &mut const_writes, tb, at + 2, argc)?;
                    if next_block != Some(to_idx(tb)) {
                        let disp_at = x64::jmp_rel32(&mut code);
                        patches.push(Rel32Patch { disp_at, target_block: tb });
                    }
                }
                Op::Br => {
                    let at = to_idx(at_u32(view.data, inst));
                    let cv = at_u32(view.extra, at);
                    let then_b = at_u32(view.extra, at + 1);
                    let then_argc = to_idx(at_u32(view.extra, at + 2));
                    let else_at = at + 3 + then_argc;
                    let else_b = at_u32(view.extra, else_at);
                    let else_argc = to_idx(at_u32(view.extra, else_at + 1));
                    let cc = match at_fusion(&fusion, inst) {
                        X64Fusion::CmpBr { cond, a, b: bv, op_ty } => {
                            emit_cmp_x64(&mut code, cond, a, bv, op_ty)?;
                            cc_of(cond)
                        }
                        X64Fusion::TestImmBr { val, mask_imm, w64, on_nonzero, .. } => {
                            let rv = load_gpr(&mut code, val, R10)?;
                            let imm = trunc_u32(at_opt(&const_u64, to_idx(mask_imm)).ok_or(TierError::Refused {
                                tier: TIER,
                                inst: Some(u32_of(inst)),
                                what: "missing test imm32 mask",
                            })?);
                            x64::test_ri(&mut code, w64, rv, imm);
                            if on_nonzero { Cc::Ne } else { Cc::E }
                        }
                        _ => {
                            let rc = load_gpr(&mut code, cv, R10)?;
                            x64::test_rr(&mut code, false, rc, rc);
                            Cc::Ne
                        }
                    };
                    if then_argc == 0 && else_argc == 0 {
                        if next_block == Some(to_idx(else_b)) {
                            let d = x64::jcc_rel32(&mut code, cc);
                            patches.push(Rel32Patch { disp_at: d, target_block: then_b });
                        } else if next_block == Some(to_idx(then_b)) {
                            let d = x64::jcc_rel32(&mut code, cc.invert());
                            patches.push(Rel32Patch { disp_at: d, target_block: else_b });
                        } else {
                            let d1 = x64::jcc_rel32(&mut code, cc);
                            patches.push(Rel32Patch { disp_at: d1, target_block: then_b });
                            let d2 = x64::jmp_rel32(&mut code);
                            patches.push(Rel32Patch { disp_at: d2, target_block: else_b });
                        }
                    } else {
                        let skip_then = x64::jcc_rel32(&mut code, cc.invert());
                        emit_edge_x64(&mut code, &mut gpr_moves, &mut xmm_moves, &mut const_writes, then_b, at + 3, then_argc)?;
                        let jmp_t = x64::jmp_rel32(&mut code);
                        patches.push(Rel32Patch { disp_at: jmp_t, target_block: then_b });
                        let else_off = code.len();
                        patch_rel32(&mut code, skip_then, else_off);
                        emit_edge_x64(&mut code, &mut gpr_moves, &mut xmm_moves, &mut const_writes, else_b, else_at + 2, else_argc)?;
                        if next_block != Some(to_idx(else_b)) {
                            let jmp_e = x64::jmp_rel32(&mut code);
                            patches.push(Rel32Patch { disp_at: jmp_e, target_block: else_b });
                        }
                    }
                }
                Op::Return => {
                    let at = to_idx(at_u32(view.data, inst));
                    let retc = to_idx(at_u32(view.extra, at));
                    if retc == 2 {
                        gpr_moves.clear();
                        const_writes.clear();
                        for ridx in 0..2usize {
                            let rv = at_u32(view.extra, at + 1 + ridx);
                            let rvi = to_idx(rv);
                            let dst = X64Loc::Gpr(if ridx == 0 { RAX } else { RDX });
                            let src = at_loc(&loc_of, rvi);
                            if src == X64Loc::None {
                                const_writes.push((dst, at_opt(&const_u64, rvi).unwrap_or(0)));
                            } else {
                                gpr_moves.push((dst, src));
                            }
                        }
                        emit_parallel_x64_gpr(&mut code, &mut gpr_moves, spill_base);
                        for &(dst, val) in const_writes.iter() {
                            if let X64Loc::Gpr(r) = dst {
                                x64::mov_imm64(&mut code, r, val);
                            }
                        }
                    } else if retc == 1 {
                        let rv = at_u32(view.extra, at + 1);
                        let rvi = to_idx(rv);
                        if let Some(double) = is_fp_ty(ty_at(view.tys, rvi)) {
                            match at_loc(&loc_of, rvi) {
                                X64Loc::Xmm(s) if s != 0 => x64::movfp_rr(&mut code, double, 0, s),
                                X64Loc::XmmSpill(s) => x64::load_fp_disp(&mut code, double, 0, RSP, spill_off(spill_base, s)),
                                _ => {}
                            }
                        } else {
                            match at_loc(&loc_of, rvi) {
                                X64Loc::Gpr(s) if s != RAX => x64::mov_rr(&mut code, true, RAX, s),
                                X64Loc::GprSpill(s) => x64::load_disp(&mut code, 8, RAX, RSP, spill_off(spill_base, s)),
                                X64Loc::None => x64::mov_imm64(&mut code, RAX, at_opt(&const_u64, rvi).unwrap_or(0)),
                                _ => {}
                            }
                        }
                    }
                    emit_epilogue(&mut code);
                    x64::ret(&mut code);
                }
                Op::Trap | Op::Unreachable => x64::ud2(&mut code),
                _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: op.name() }),
            }
            inst += 1;
        }
    }

    // 5. Patch all intra-function rel32 jump displacements.
    for p in &patches {
        let target_off = at_usize(&block_byte_off, to_idx(p.target_block));
        patch_rel32(&mut code, p.disp_at, target_off);
    }

    out.code = code;
    Ok(out)
}
