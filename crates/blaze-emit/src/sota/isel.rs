//! ISLE-Style Bottom-Up Pattern-Fusing AArch64 Emitter for `TierName::Sota`.
//!
//! Emits native AArch64 machine code directly from verified, `aegraph` + LICM + strength-reduced
//! `blaze-lir`:
//!
//! - **Fused Multiply-Add / Multiply-Sub**: `add(mul(a, b), c)` -> `madd`, `sub(c, mul(a, b))` -> `msub`,
//!   plus contractible FP `fadd(fmul(a, b), c)` -> `fmadd`, `fsub(c, fmul(a, b))` -> `fmsub`, and `fma`/`fmuladd`
//! - **Fused Shifted-Register ALU**: `add(a, shl(b, k))` -> `add lsl #k`, `add(a, lsr(b, k))` -> `add lsr #k`, `sub(a, shl(b, k))` -> `sub lsl #k`
//! - **Address-Mode Folding**: `load`/`store` on `slot_addr(slot)` -> `[sp, #slot_off + off]` (eliminating `slot_addr` when all uses fold),
//!   and `load`/`store` on `add`/`ptr_add(base, imm)` -> `[base, #imm + off]`
//! - **Zero-Materialization Immediates**: `add_imm`, `sub_imm`, `cmp_imm`, `lsl_imm`, `lsr_imm`, `sbfm` (`asr #k`), and `ubfm` (`ubfx`) for power-of-2 bitmasks (`x & (2^k - 1)`)
//! - **Fused Compare + Branch / Select**: `icmp`/`fcmp + br` -> `cmp`/`fcmp`/`cbz` + `b.cond`; `icmp`/`fcmp + select` -> `cmp`/`fcmp` + `csel`/`fcsel`
//! - **Spill-Capable Parallel-Copy Resolver**: optimal block-param, entry-param & call-arg register/spill shuffling with `X16`/`V31` cycle breakers + fallthrough branch elimination
//! - **Scalar FP (`f32`/`f64`) + Stack Slots + External `DataAddr`/`FuncAddr` + `Switch` + `CallIndirect`**
//! - **Frameless Leaf Functions**: zero `stp`/`ldp`/`sp` traffic when no calls, callee-saved regs, stack slots, or spills are needed.

use blaze_code::record::{OWN_RELRO, Space, Symbol, Target};
use blaze_lir::{
    Block, Flags, FloatCond, IntCond, Op, Ty, View,
    func::{Conv, DataId, Ext, ParamKind, RefId, RefKind, SigId},
};
use blaze_own::{Alloc, Vec};
use blaze_tier::{Emitted, Named, Names, Outside, Reloc, RelocKind, TierError, TierName};

use super::regalloc::{self, ARG_REGS, AllocLoc, FP_ARG_REGS, is_fp_ty};
use crate::enc::{self, Cond, FP, Ftype, LR, Reg, SP, Size, X0, X16, ZR};
use crate::enc_probe;
use crate::pass::{BRK_DIV_OVERFLOW, BRK_DIV_ZERO, BRK_UNREACHABLE, probe::PROBE_STEP};

const X17: Reg = 17;
/// Permanently reserved FP scratch registers (never allocated to SSA values).
const V29: Reg = 29;
const V30: Reg = 30;
const V31: Reg = 31;

const TIER: TierName = TierName::Sota;
/// Maximum stack frame size supported (16 MiB, matching `pass.rs`).
const FRAME_LIMIT: u64 = 1 << 24;
/// Maximum stack slot alignment (4 KiB = 2^12, matching `pass.rs`).
const MOST_SLOT_ALIGN: u8 = 12;
/// Maximum own-data alignment (4 KiB = 2^12, matching `pass.rs`).
const MOST_DATA_ALIGN: u32 = blaze_code::record::MOST_PART_ALIGN as u32;

/// Fused instruction selection pattern chosen for an instruction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fusion {
    None,
    /// `add(mul(a, b), c)` -> `madd rd, a, b, c`.
    Madd { a: u32, b: u32, acc: u32 },
    /// `sub(c, mul(a, b))` -> `msub rd, a, b, c`.
    Msub { a: u32, b: u32, acc: u32 },
    /// Contractible `fadd(fmul(a, b), c)` -> `fmadd vd, a, b, c`.
    Fmadd { a: u32, b: u32, acc: u32 },
    /// Contractible `fsub(c, fmul(a, b))` -> `fmsub vd, a, b, c`.
    Fmsub { a: u32, b: u32, acc: u32 },
    /// `add(a, shl(b, k))` -> `add rd, a, b, lsl #k`.
    AddLsl { a: u32, b: u32, shift: u32, imm_inst: u32 },
    /// `add(a, lshr(b, k))` -> `add rd, a, b, lsr #k`.
    AddLsr { a: u32, b: u32, shift: u32, imm_inst: u32 },
    /// `sub(a, shl(b, k))` -> `sub rd, a, b, lsl #k`.
    SubLsl { a: u32, b: u32, shift: u32, imm_inst: u32 },
    /// `br(icmp(cond, a, b), then, else)` -> fused `cmp`/`cbz` + `b.cond`.
    CmpBr { cond: IntCond, a: u32, b: u32, op_ty: Ty },
    /// `br(fcmp(cond, a, b), then, else)` -> fused `fcmp` + `b.cond`.
    FcmpBr { cond: Cond, a: u32, b: u32, double: bool },
    /// `select(icmp(cond, a, b), t, f)` -> fused `cmp` + `csel`/`fcsel`.
    CmpSelect { cond: IntCond, a: u32, b: u32, op_ty: Ty },
    /// `select(fcmp(cond, a, b), t, f)` -> fused `fcmp` + `csel`/`fcsel`.
    FcmpSelect { cond: Cond, a: u32, b: u32, double: bool },
    /// `load`/`store` directly from `[sp, #off]` (folded `slot_addr`).
    MemSpOff { off: i64 },
    /// `load`/`store` directly from `[base, #off]` (folded `add`/`ptr_add` with constant).
    MemBaseOff { base: u32, off: i64, imm_inst: u32 },
}

/// Branch patch site resolved once every block's word offset is known.
#[derive(Clone, Copy, Debug)]
enum PatchKind {
    B,
    BCond(Cond),
    Cbz(Reg),
    Cbnz(Reg),
}

#[derive(Clone, Copy, Debug)]
struct BranchPatch {
    word_idx: usize,
    kind: PatchKind,
    target_block: u32,
}

/// Source or destination location for a GPR parallel move (register or 8-byte spill slot).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GprCopyLoc {
    Reg(Reg),
    Spill(u32),
}

/// Source or destination location for an FP parallel move (register or 8-byte spill slot).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FpCopyLoc {
    Reg(Reg),
    Spill(u32),
}

#[inline]
fn is_supported_scalar(ty: Ty) -> bool {
    if ty == Ty::NONE {
        return true;
    }
    ty.is_ptr() || is_fp_ty(ty).is_some() || (ty.is_scalar() && ty.is_int() && ty.bits(64) <= 64)
}

#[inline]
fn mem_size_of(ty: Ty) -> Option<(Size, bool)> {
    if let Some(double) = is_fp_ty(ty) {
        return Some((if double { Size::X } else { Size::W }, true));
    }
    if ty.is_ptr() {
        return Some((Size::X, false));
    }
    if ty.is_scalar() && ty.is_int() {
        let size = match ty.bits(64) {
            8 => Size::B,
            16 => Size::H,
            32 => Size::W,
            64 => Size::X,
            _ => return None,
        };
        return Some((size, false));
    }
    None
}

#[inline]
fn fits_mem_off(size: Size, off: i64) -> bool {
    let bytes = 1i64 << (size as u32);
    (off >= 0 && off % bytes == 0 && off / bytes < 4096) || (-256..=255).contains(&off)
}

#[inline]
fn extract_const_u64(view: &View<'_>, inst: usize) -> Option<u64> {
    let op = view.op(inst);
    let ty = Ty(view.tys[inst]);
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
            let w = view.data[inst];
            let v = if bits <= 32 {
                u64::from(w)
            } else {
                (w as i32 as i64) as u64
            };
            Some(v & mask)
        }
        Op::IconstWide => {
            let at = view.data[inst] as usize;
            let lo = u64::from(view.extra[at]);
            let hi = if ty.const_words() >= 2 { u64::from(view.extra[at + 1]) } else { 0 };
            Some((lo | (hi << 32)) & mask)
        }
        _ => None,
    }
}

#[inline]
fn sign_extend_u64(val: u64, bits: u32) -> i64 {
    if bits >= 64 {
        val as i64
    } else {
        let shift = 64 - bits;
        ((val << shift) as i64) >> shift
    }
}

#[inline]
fn is_low_bitmask(v: u64) -> Option<u32> {
    if v == 0 || v == u64::MAX {
        return None;
    }
    let next = v.wrapping_add(1);
    if next.is_power_of_two() {
        let k = next.trailing_zeros();
        if (1..=63).contains(&k) {
            return Some(k);
        }
    }
    None
}

#[inline]
fn cond_of(c: IntCond) -> Cond {
    match c {
        IntCond::Eq => Cond::Eq,
        IntCond::Ne => Cond::Ne,
        IntCond::Ult => Cond::Lo,
        IntCond::Ule => Cond::Ls,
        IntCond::Ugt => Cond::Hi,
        IntCond::Uge => Cond::Hs,
        IntCond::Slt => Cond::Lt,
        IntCond::Sle => Cond::Le,
        IntCond::Sgt => Cond::Gt,
        IntCond::Sge => Cond::Ge,
    }
}

#[inline]
fn fcond_of(c: FloatCond) -> Option<Cond> {
    match c {
        FloatCond::Oeq => Some(Cond::Eq),
        FloatCond::Une => Some(Cond::Ne),
        FloatCond::Olt => Some(Cond::Mi),
        FloatCond::Ole => Some(Cond::Ls),
        FloatCond::Ogt => Some(Cond::Gt),
        FloatCond::Oge => Some(Cond::Ge),
        FloatCond::Ord => Some(Cond::Vc),
        FloatCond::Uno => Some(Cond::Vs),
        FloatCond::Ult => Some(Cond::Lt),
        FloatCond::Ule => Some(Cond::Le),
        FloatCond::Ugt => Some(Cond::Hi),
        FloatCond::Uge => Some(Cond::Pl),
        FloatCond::False | FloatCond::True | FloatCond::One | FloatCond::Ueq => None,
    }
}

#[inline]
fn invert_cond(c: Cond) -> Cond {
    match c {
        Cond::Eq => Cond::Ne,
        Cond::Ne => Cond::Eq,
        Cond::Hs => Cond::Lo,
        Cond::Lo => Cond::Hs,
        Cond::Mi => Cond::Pl,
        Cond::Pl => Cond::Mi,
        Cond::Vs => Cond::Vc,
        Cond::Vc => Cond::Vs,
        Cond::Hi => Cond::Ls,
        Cond::Ls => Cond::Hi,
        Cond::Ge => Cond::Lt,
        Cond::Lt => Cond::Ge,
        Cond::Gt => Cond::Le,
        Cond::Le => Cond::Gt,
        Cond::Al => Cond::Nv,
        Cond::Nv => Cond::Al,
    }
}

/// Emits a 64-bit constant into `rd` using minimal `movz`/`movn`/`movk` instructions.
fn emit_const_u64(words: &mut Vec<u32, Alloc<'_>>, rd: Reg, val: u64) {
    if val == 0 {
        words.push(enc::movz(rd, 0, 0));
        return;
    }
    let inv = !val;
    for hw in 0..4u32 {
        let chunk = ((inv >> (hw * 16)) & 0xffff) as u32;
        if inv == (u64::from(chunk) << (hw * 16)) {
            words.push(enc::movn(rd, chunk, hw));
            return;
        }
    }
    let mut first = true;
    for hw in 0..4u32 {
        let chunk = ((val >> (hw * 16)) & 0xffff) as u32;
        if chunk != 0 {
            if first {
                words.push(enc::movz(rd, chunk, hw));
                first = false;
            } else {
                words.push(enc::movk(rd, chunk, hw));
            }
        }
    }
}

/// Emits `rd = sp + off` for `off < FRAME_LIMIT` (16 MiB).
#[inline]
fn emit_sp_addr(words: &mut Vec<u32, Alloc<'_>>, rd: Reg, off: u32) {
    let (hi, lo) = (off >> 12, off & 0xfff);
    if hi == 0 {
        words.push(enc::add_imm(rd, SP, lo, false));
    } else {
        words.push(enc::add_imm(rd, SP, hi, true));
        if lo != 0 {
            words.push(enc::add_imm(rd, rd, lo, false));
        }
    }
}

/// Narrow `rd` to `bits` zero-extended when `bits < 64`.
#[inline]
fn emit_narrow(words: &mut Vec<u32, Alloc<'_>>, rd: Reg, bits: u32) {
    if bits < 64 {
        words.push(enc::zext(rd, rd, bits));
    }
}

/// Emits a single `ldr`/`str` or `ldur`/`stur` (GPR or FP) for an offset guaranteed to fit [`fits_mem_off`].
#[inline]
fn emit_mem_imm(words: &mut Vec<u32, Alloc<'_>>, load: bool, fp: bool, size: Size, rt: Reg, rn: Reg, off: i64) {
    let bytes = 1i64 << (size as u32);
    let raw = if off >= 0 && off % bytes == 0 && off / bytes < 4096 {
        let imm12 = (off / bytes) as u32;
        if load {
            enc::ldr(size, rt, rn, imm12)
        } else {
            enc::str(size, rt, rn, imm12)
        }
    } else {
        debug_assert!((-256..=255).contains(&off));
        if load {
            enc::ldur(size, rt, rn, off as i32)
        } else {
            enc::stur(size, rt, rn, off as i32)
        }
    };
    words.push(if fp { enc::fp_access(raw) } else { raw });
}

/// Loads an 8-byte GPR spill slot at `sp + spill_base + slot * 8` (`< 32768`) into `rd`.
#[inline]
fn emit_ld_spill_gpr(words: &mut Vec<u32, Alloc<'_>>, rd: Reg, spill_base: u32, slot: u32) {
    let off = spill_base + slot * 8;
    debug_assert!(off % 8 == 0 && off / 8 < 4096);
    words.push(enc::ldr(Size::X, rd, SP, off / 8));
}

/// Stores `rs` to an 8-byte GPR spill slot at `sp + spill_base + slot * 8` (`< 32768`).
#[inline]
fn emit_st_spill_gpr(words: &mut Vec<u32, Alloc<'_>>, rs: Reg, spill_base: u32, slot: u32) {
    let off = spill_base + slot * 8;
    debug_assert!(off % 8 == 0 && off / 8 < 4096);
    words.push(enc::str(Size::X, rs, SP, off / 8));
}

/// Loads an 8-byte FP spill slot as `d` (preserves low 32 bits for `f32` and all 64 bits for `f64`).
#[inline]
fn emit_ld_spill_fp(words: &mut Vec<u32, Alloc<'_>>, vd: Reg, spill_base: u32, slot: u32) {
    let off = spill_base + slot * 8;
    debug_assert!(off % 8 == 0 && off / 8 < 4096);
    words.push(enc::fp_access(enc::ldr(Size::X, vd, SP, off / 8)));
}

/// Stores `d` to an 8-byte FP spill slot.
#[inline]
fn emit_st_spill_fp(words: &mut Vec<u32, Alloc<'_>>, vs: Reg, spill_base: u32, slot: u32) {
    let off = spill_base + slot * 8;
    debug_assert!(off % 8 == 0 && off / 8 < 4096);
    words.push(enc::fp_access(enc::str(Size::X, vs, SP, off / 8)));
}

/// Resolves an arbitrary simultaneous parallel GPR copy `[(dst, src)]` over registers and spill slots,
/// using `X16` as cycle breaker and `X17` as spill-to-spill transfer scratch.
fn emit_parallel_gpr_moves(
    words: &mut Vec<u32, Alloc<'_>>,
    moves: &mut Vec<(GprCopyLoc, GprCopyLoc), Alloc<'_>>,
    spill_base: u32,
) {
    let mut i = 0;
    while i < moves.len() {
        if moves[i].0 == moves[i].1 {
            moves.remove_swap(i);
        } else {
            i += 1;
        }
    }

    let emit_one = |words: &mut Vec<u32, Alloc<'_>>, dst: GprCopyLoc, src: GprCopyLoc| match (dst, src) {
        (GprCopyLoc::Reg(d), GprCopyLoc::Reg(s)) => {
            if d != s {
                words.push(enc::mov(d, s));
            }
        }
        (GprCopyLoc::Reg(d), GprCopyLoc::Spill(s)) => {
            emit_ld_spill_gpr(words, d, spill_base, s);
        }
        (GprCopyLoc::Spill(d), GprCopyLoc::Reg(s)) => {
            emit_st_spill_gpr(words, s, spill_base, d);
        }
        (GprCopyLoc::Spill(d), GprCopyLoc::Spill(s)) => {
            emit_ld_spill_gpr(words, X17, spill_base, s);
            emit_st_spill_gpr(words, X17, spill_base, d);
        }
    };

    while !moves.is_empty() {
        let mut ready = None;
        for idx in 0..moves.len() {
            let dst = moves[idx].0;
            if !moves.iter().any(|&(_, s)| s == dst) {
                ready = Some(idx);
                break;
            }
        }
        if let Some(idx) = ready {
            let (dst, src) = moves.remove_swap(idx);
            emit_one(words, dst, src);
        } else {
            // Pure cycle: save `moves[0].0` in `X16`, redirect all reads of `moves[0].0` to `X16`.
            // Note: `emit_one` uses only `X17` for spill-to-spill, never touching `X16`.
            let (dst, src) = moves.remove_swap(0);
            match dst {
                GprCopyLoc::Reg(d) => words.push(enc::mov(X16, d)),
                GprCopyLoc::Spill(s) => emit_ld_spill_gpr(words, X16, spill_base, s),
            }
            for m in moves.iter_mut() {
                if m.1 == dst {
                    m.1 = GprCopyLoc::Reg(X16);
                }
            }
            emit_one(words, dst, src);
        }
    }
}

/// Resolves an arbitrary simultaneous parallel FP copy `[(dst, src)]` over registers and spill slots,
/// using `V31` as cycle breaker and `V30` as spill-to-spill transfer scratch.
fn emit_parallel_fp_moves(
    words: &mut Vec<u32, Alloc<'_>>,
    moves: &mut Vec<(FpCopyLoc, FpCopyLoc), Alloc<'_>>,
    spill_base: u32,
) {
    let mut i = 0;
    while i < moves.len() {
        if moves[i].0 == moves[i].1 {
            moves.remove_swap(i);
        } else {
            i += 1;
        }
    }

    let emit_one = |words: &mut Vec<u32, Alloc<'_>>, dst: FpCopyLoc, src: FpCopyLoc| match (dst, src) {
        (FpCopyLoc::Reg(d), FpCopyLoc::Reg(s)) => {
            if d != s {
                words.push(enc::fmov_reg(true, d, s));
            }
        }
        (FpCopyLoc::Reg(d), FpCopyLoc::Spill(s)) => {
            emit_ld_spill_fp(words, d, spill_base, s);
        }
        (FpCopyLoc::Spill(d), FpCopyLoc::Reg(s)) => {
            emit_st_spill_fp(words, s, spill_base, d);
        }
        (FpCopyLoc::Spill(d), FpCopyLoc::Spill(s)) => {
            emit_ld_spill_fp(words, V30, spill_base, s);
            emit_st_spill_fp(words, V30, spill_base, d);
        }
    };

    while !moves.is_empty() {
        let mut ready = None;
        for idx in 0..moves.len() {
            let dst = moves[idx].0;
            if !moves.iter().any(|&(_, s)| s == dst) {
                ready = Some(idx);
                break;
            }
        }
        if let Some(idx) = ready {
            let (dst, src) = moves.remove_swap(idx);
            emit_one(words, dst, src);
        } else {
            let (dst, src) = moves.remove_swap(0);
            match dst {
                FpCopyLoc::Reg(d) => words.push(enc::fmov_reg(true, V31, d)),
                FpCopyLoc::Spill(s) => emit_ld_spill_fp(words, V31, spill_base, s),
            }
            for m in moves.iter_mut() {
                if m.1 == dst {
                    m.1 = FpCopyLoc::Reg(V31);
                }
            }
            emit_one(words, dst, src);
        }
    }
}

/// Validates a C calling-convention signature within `Sota`'s scalar register-passed ABI subset.
fn check_sota_sig(view: &View<'_>, sig_id: SigId, inst: Option<u32>) -> Result<(), TierError> {
    let Some(rec) = view.sigs.get(sig_id.index()) else {
        return Err(TierError::Refused { tier: TIER, inst, what: "missing signature" });
    };
    if rec.conv != Conv::C as u8 {
        return Err(TierError::Refused { tier: TIER, inst, what: "non-C calling convention" });
    }
    if rec.bits & blaze_lir::func::SigRec::VARIADIC != 0 {
        return Err(TierError::Refused { tier: TIER, inst, what: "variadic signature" });
    }
    let sig = view.sig(sig_id);
    let mut gpr_n = 0usize;
    let mut fp_n = 0usize;
    for p in sig.params {
        if p.kind != ParamKind::Value as u8 {
            return Err(TierError::Refused { tier: TIER, inst, what: "sret/byval parameter" });
        }
        let ty = Ty(p.ty);
        if !is_supported_scalar(ty) || ty == Ty::NONE {
            return Err(TierError::Refused { tier: TIER, inst, what: "unsupported parameter type" });
        }
        if is_fp_ty(ty).is_some() {
            fp_n += 1;
        } else {
            gpr_n += 1;
        }
    }
    if gpr_n > ARG_REGS.len() || fp_n > FP_ARG_REGS.len() {
        return Err(TierError::Refused { tier: TIER, inst, what: "stacked parameters (>8 GPR or >8 FP)" });
    }
    if sig.rets.len() > 1 {
        return Err(TierError::Refused { tier: TIER, inst, what: "multi-value return" });
    }
    for r in sig.rets {
        if r.kind != ParamKind::Value as u8 {
            return Err(TierError::Refused { tier: TIER, inst, what: "non-value return" });
        }
        let ty = Ty(r.ty);
        if !is_supported_scalar(ty) || ty == Ty::NONE {
            return Err(TierError::Refused { tier: TIER, inst, what: "unsupported return type" });
        }
    }
    Ok(())
}

/// Validates that `view` is inside `Sota`'s direct AArch64 fused subset, returning `Err` naming the
/// exact offending instruction so `Sota::with_fallback` can cleanly delegate exotic ops.
fn check_subset(view: &View<'_>) -> Result<(), TierError> {
    if view.head.ptr_bits != 64 {
        return Err(TierError::Refused { tier: TIER, inst: None, what: "non-64-bit pointers" });
    }
    if view.blocks() == 0 {
        return Err(TierError::Refused { tier: TIER, inst: None, what: "empty function" });
    }
    for b in 0..view.blocks() {
        if view.block_kind(Block(b as u32)).is_landing() {
            return Err(TierError::Refused { tier: TIER, inst: None, what: "unwind landing block" });
        }
    }
    check_sota_sig(view, SigId(view.head.sig), None)?;
    for slot in view.slots {
        if slot.align > MOST_SLOT_ALIGN {
            return Err(TierError::Refused { tier: TIER, inst: None, what: "slot alignment > 4 KiB" });
        }
    }

    for inst in 0..view.len() {
        let ty = Ty(view.tys[inst]);
        if !is_supported_scalar(ty) {
            return Err(TierError::Refused { tier: TIER, inst: Some(inst as u32), what: "non-scalar-64/fp type" });
        }
        let op = view.op(inst);
        let flags = Flags(view.flags[inst]);
        let honoured = match op {
            Op::Load | Op::Store => Flags::VOLATILE.0,
            _ => 0,
        };
        if op.info().flags.meanings_only(flags).0 & !honoured != 0 {
            return Err(TierError::Refused { tier: TIER, inst: Some(inst as u32), what: "unhonoured meaning flag" });
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
                | Op::Add
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
                | Op::Rotr
                | Op::Icmp
                | Op::Fcmp
                | Op::Select
                | Op::Zext
                | Op::Sext
                | Op::Trunc
                | Op::Fpext
                | Op::Fptrunc
                | Op::FptoSi
                | Op::FptoUi
                | Op::SitoFp
                | Op::UitoFp
                | Op::Bitcast
                | Op::Fadd
                | Op::Fsub
                | Op::Fmul
                | Op::Fdiv
                | Op::Fneg
                | Op::Fabs
                | Op::Sqrt
                | Op::Floor
                | Op::Ceil
                | Op::Ftrunc
                | Op::Nearest
                | Op::Fround
                | Op::FminNum
                | Op::FmaxNum
                | Op::Fminimum
                | Op::Fmaximum
                | Op::Fma
                | Op::Fmuladd
                | Op::Freeze
                | Op::Assume
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
                | Op::DataAddr
                | Op::FuncAddr
                | Op::Jump
                | Op::Br
                | Op::Switch
                | Op::Return
                | Op::Trap
                | Op::Unreachable
                | Op::Call
                | Op::CallIndirect
        );
        if !ok {
            return Err(TierError::Refused { tier: TIER, inst: Some(inst as u32), what: op.name() });
        }

        match op {
            Op::Load => {
                if mem_size_of(ty).is_none() {
                    return Err(TierError::Refused { tier: TIER, inst: Some(inst as u32), what: "load of non-byte width" });
                }
            }
            Op::Store => {
                let at = view.data[inst] as usize;
                let val_ty = Ty(view.tys[view.extra[at] as usize]);
                if mem_size_of(val_ty).is_none() {
                    return Err(TierError::Refused { tier: TIER, inst: Some(inst as u32), what: "store of non-byte width" });
                }
            }
            Op::Fconst => {
                if ty != Ty::F32 {
                    return Err(TierError::Refused { tier: TIER, inst: Some(inst as u32), what: "non-f32 fconst" });
                }
            }
            Op::FconstWide => {
                if ty != Ty::F64 {
                    return Err(TierError::Refused { tier: TIER, inst: Some(inst as u32), what: "non-f64 fconst_wide" });
                }
            }
            Op::FptoSi | Op::FptoUi => {
                let bits = ty.bits(64);
                if bits < 8 || bits > 64 {
                    return Err(TierError::Refused { tier: TIER, inst: Some(inst as u32), what: "fptoi to i1" });
                }
            }
            Op::Bitcast => {
                let src_ty = Ty(view.tys[view.data[inst] as usize]);
                let bits_of = |t: Ty| -> Option<u32> {
                    if let Some(d) = is_fp_ty(t) {
                        Some(if d { 64 } else { 32 })
                    } else if t.is_scalar() && t.is_int() && t.bits(64) <= 64 {
                        Some(t.bits(64))
                    } else {
                        None
                    }
                };
                match (bits_of(src_ty), bits_of(ty)) {
                    (Some(a), Some(b)) if a == b => {}
                    _ => {
                        return Err(TierError::Refused { tier: TIER, inst: Some(inst as u32), what: "unsupported bitcast" });
                    }
                }
            }
            Op::Rotr => {
                if ty.bits(64) != 64 {
                    return Err(TierError::Refused { tier: TIER, inst: Some(inst as u32), what: "sub-64 rotr" });
                }
            }
            Op::Trap => {
                if view.data[inst] >= BRK_DIV_ZERO {
                    return Err(TierError::Refused { tier: TIER, inst: Some(inst as u32), what: "reserved trap code" });
                }
            }
            Op::DataAddr => {
                let rec = view.reference(RefId(view.data[inst]));
                match RefKind::from_word(rec.kind) {
                    Some(RefKind::Static | RefKind::Vtable) => {}
                    Some(RefKind::Data) => {
                        let row = rec.place;
                        let data = view.datas[row as usize];
                        if data.align > MOST_DATA_ALIGN {
                            return Err(TierError::Refused { tier: TIER, inst: Some(inst as u32), what: "data align > 4 KiB" });
                        }
                        if !view.data_relocs_of(DataId(row)).is_empty()
                            || !view.data_positions_of(DataId(row)).is_empty()
                        {
                            return Err(TierError::Refused { tier: TIER, inst: Some(inst as u32), what: "own data with relocs/positions" });
                        }
                    }
                    _ => {
                        return Err(TierError::Refused { tier: TIER, inst: Some(inst as u32), what: "unsupported data_addr kind" });
                    }
                }
            }
            Op::FuncAddr => {
                let rec = view.reference(RefId(view.data[inst]));
                if rec.kind != RefKind::Func as u32 {
                    return Err(TierError::Refused { tier: TIER, inst: Some(inst as u32), what: "non-func func_addr" });
                }
            }
            Op::Call | Op::CallIndirect => {
                let at = view.data[inst] as usize;
                if op == Op::Call {
                    let rec = view.reference(RefId(view.extra[at]));
                    if rec.kind != RefKind::Func as u32 {
                        return Err(TierError::Refused { tier: TIER, inst: Some(inst as u32), what: "non-func call target" });
                    }
                }
                let sig_id = SigId(view.extra[at + 1]);
                check_sota_sig(view, sig_id, Some(inst as u32))?;
                let argc = view.extra[at + 2] as usize;
                if argc != view.sig(sig_id).params.len() {
                    return Err(TierError::Refused { tier: TIER, inst: Some(inst as u32), what: "call argc mismatch" });
                }
            }
            Op::Return => {
                let at = view.data[inst] as usize;
                let retc = view.extra[at] as usize;
                if retc != view.sig(SigId(view.head.sig)).rets.len() {
                    return Err(TierError::Refused { tier: TIER, inst: Some(inst as u32), what: "return arity mismatch" });
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Resolves an external symbol or C name into `out.targets` (and `out.names`/`out.text`).
fn resolve_outside<'a>(
    names: &dyn Names,
    of: Outside,
    space: Space,
    allow_c_name: bool,
    out: &mut Emitted<'a>,
    inst: usize,
) -> Result<u32, TierError> {
    match names.name(of) {
        Some(Named::Symbol(symbol)) => {
            let idx = out.targets.len() as u32;
            out.targets.push(Target { space, symbol });
            Ok(idx)
        }
        Some(Named::C(name)) => {
            if !allow_c_name {
                return Err(TierError::Refused { tier: TIER, inst: Some(inst as u32), what: "C name for non-func/static" });
            }
            let sym = Symbol::of_name(name);
            let start = out.text.len() as u32;
            out.text.extend_from_slice(name.as_bytes());
            out.names.push((sym, start, name.len() as u32));
            let idx = out.targets.len() as u32;
            out.targets.push(Target { space: Space::Outside, symbol: sym });
            Ok(idx)
        }
        None => Err(TierError::NoName { tier: TIER, of }),
    }
}

/// Compiles verified, optimized `view` directly into fused AArch64 machine code in an `Emitted`.
pub(crate) fn emit_sota_a64<'a>(
    view: View<'_>,
    names: &dyn Names,
    alloc: Alloc<'a>,
) -> Result<Emitted<'a>, TierError> {
    check_subset(&view)?;

    let len = view.len();
    let blocks = view.blocks();

    // 0. Compute LIR stack slot layout upfront (placed at `sp + 0..spill_base`) so `slot_addr`
    //    folding into `[sp, #slot_off + off]` happens BEFORE register allocation!
    let mut most_align: u64 = 16;
    let mut slot_cursor: u64 = 0;
    let mut slot_at: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(view.slots.len(), alloc);
    for slot in view.slots {
        let align = 1u64 << slot.align;
        most_align = most_align.max(align);
        slot_cursor = slot_cursor.next_multiple_of(align);
        if slot_cursor >= FRAME_LIMIT {
            return Err(TierError::Refused { tier: TIER, inst: None, what: "stack frame >= 16 MiB" });
        }
        slot_at.push(slot_cursor as u32);
        slot_cursor += u64::from(slot.size);
        if slot_cursor >= FRAME_LIMIT {
            return Err(TierError::Refused { tier: TIER, inst: None, what: "stack frame >= 16 MiB" });
        }
    }
    let spill_base_u64 = slot_cursor.next_multiple_of(8);
    if spill_base_u64 >= FRAME_LIMIT {
        return Err(TierError::Refused { tier: TIER, inst: None, what: "stack frame >= 16 MiB" });
    }
    let spill_base = spill_base_u64 as u32;
    // All spill slots use single-instruction `[sp, #imm12*8]` accesses (< 32 KiB).
    let max_spill_slots = if spill_base < 32760 { (32760 - spill_base) / 8 } else { 0 };

    // 1. Precompute block ownership, use counts, and inline integer constants.
    let mut inst_block: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    let mut use_count: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    let mut const_u64: Vec<Option<u64>, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    for inst in 0..len {
        inst_block.push(0);
        use_count.push(0);
        const_u64.push(extract_const_u64(&view, inst));
    }
    for b in 0..blocks {
        for inst in view.block(Block(b as u32)) {
            inst_block[inst] = b as u32;
            regalloc::for_each_use(&view, inst, |v| {
                let vi = v as usize;
                if vi < len {
                    use_count[vi] = use_count[vi].saturating_add(1);
                }
            });
        }
    }

    // 2a. ISLE-style pattern fusion detection (sets `fusion[inst]` and marks fused producers in `skip_emit`).
    let mut fusion: Vec<Fusion, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    let mut skip_emit: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    let mut slot_addr_folded_uses: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    for _ in 0..len {
        fusion.push(Fusion::None);
        skip_emit.push(false);
        slot_addr_folded_uses.push(0);
    }

    for inst in 0..len {
        let op = view.op(inst);
        let ty = Ty(view.tys[inst]);
        let bits = if ty == Ty::NONE || is_fp_ty(ty).is_some() { 0 } else { ty.bits(64) };
        let b = inst_block[inst];

        match op {
            Op::Add if bits == 64 => {
                let at = view.data[inst] as usize;
                let lhs = view.extra[at];
                let rhs = view.extra[at + 1];
                // Pattern 1: add(mul(a, b), c) or add(c, mul(a, b)) -> madd
                let try_mul = |prod: u32, acc: u32| -> Option<(u32, u32, u32)> {
                    let pi = prod as usize;
                    if use_count[pi] == 1 && inst_block[pi] == b && view.op(pi) == Op::Mul && !skip_emit[pi] {
                        let pat = view.data[pi] as usize;
                        Some((view.extra[pat], view.extra[pat + 1], acc))
                    } else {
                        None
                    }
                };
                if let Some((a, mb, acc)) = try_mul(lhs, rhs) {
                    fusion[inst] = Fusion::Madd { a, b: mb, acc };
                    skip_emit[lhs as usize] = true;
                    continue;
                }
                if let Some((a, mb, acc)) = try_mul(rhs, lhs) {
                    fusion[inst] = Fusion::Madd { a, b: mb, acc };
                    skip_emit[rhs as usize] = true;
                    continue;
                }
                // Pattern 2: add(a, shl(b, k)) / add(a, lshr(b, k)) -> add_lsl / add_lsr
                let try_shift = |sh: u32, other: u32| -> Option<(Op, u32, u32, u32, u32)> {
                    let si = sh as usize;
                    let sop = view.op(si);
                    if use_count[si] == 1
                        && inst_block[si] == b
                        && matches!(sop, Op::Shl | Op::Lshr)
                        && !skip_emit[si]
                    {
                        let sat = view.data[si] as usize;
                        let sb = view.extra[sat];
                        let sk = view.extra[sat + 1];
                        if let Some(k) = const_u64[sk as usize] {
                            return Some((sop, other, sb, (k & 63) as u32, sk));
                        }
                    }
                    None
                };
                if let Some((sop, a, sb, shift, sk)) = try_shift(rhs, lhs) {
                    fusion[inst] = if sop == Op::Shl {
                        Fusion::AddLsl { a, b: sb, shift, imm_inst: sk }
                    } else {
                        Fusion::AddLsr { a, b: sb, shift, imm_inst: sk }
                    };
                    skip_emit[rhs as usize] = true;
                    continue;
                }
                if let Some((sop, a, sb, shift, sk)) = try_shift(lhs, rhs) {
                    fusion[inst] = if sop == Op::Shl {
                        Fusion::AddLsl { a, b: sb, shift, imm_inst: sk }
                    } else {
                        Fusion::AddLsr { a, b: sb, shift, imm_inst: sk }
                    };
                    skip_emit[lhs as usize] = true;
                    continue;
                }
            }
            Op::Sub if bits == 64 => {
                let at = view.data[inst] as usize;
                let lhs = view.extra[at];
                let rhs = view.extra[at + 1];
                let ri = rhs as usize;
                if use_count[ri] == 1 && inst_block[ri] == b && view.op(ri) == Op::Mul && !skip_emit[ri] {
                    let pat = view.data[ri] as usize;
                    fusion[inst] = Fusion::Msub { a: view.extra[pat], b: view.extra[pat + 1], acc: lhs };
                    skip_emit[ri] = true;
                    continue;
                }
                if use_count[ri] == 1 && inst_block[ri] == b && view.op(ri) == Op::Shl && !skip_emit[ri] {
                    let sat = view.data[ri] as usize;
                    let sb = view.extra[sat];
                    let sk = view.extra[sat + 1];
                    if let Some(k) = const_u64[sk as usize] {
                        fusion[inst] = Fusion::SubLsl { a: lhs, b: sb, shift: (k & 63) as u32, imm_inst: sk };
                        skip_emit[ri] = true;
                        continue;
                    }
                }
            }
            Op::Fadd if Flags(view.flags[inst]).contains(Flags::CONTRACT) => {
                let at = view.data[inst] as usize;
                let lhs = view.extra[at];
                let rhs = view.extra[at + 1];
                let try_fmul = |prod: u32, acc: u32| -> Option<(u32, u32, u32)> {
                    let pi = prod as usize;
                    if use_count[pi] == 1
                        && inst_block[pi] == b
                        && view.op(pi) == Op::Fmul
                        && Flags(view.flags[pi]).contains(Flags::CONTRACT)
                        && !skip_emit[pi]
                    {
                        let pat = view.data[pi] as usize;
                        Some((view.extra[pat], view.extra[pat + 1], acc))
                    } else {
                        None
                    }
                };
                if let Some((a, mb, acc)) = try_fmul(lhs, rhs) {
                    fusion[inst] = Fusion::Fmadd { a, b: mb, acc };
                    skip_emit[lhs as usize] = true;
                    continue;
                }
                if let Some((a, mb, acc)) = try_fmul(rhs, lhs) {
                    fusion[inst] = Fusion::Fmadd { a, b: mb, acc };
                    skip_emit[rhs as usize] = true;
                    continue;
                }
            }
            Op::Fsub if Flags(view.flags[inst]).contains(Flags::CONTRACT) => {
                let at = view.data[inst] as usize;
                let lhs = view.extra[at];
                let rhs = view.extra[at + 1];
                let ri = rhs as usize;
                if use_count[ri] == 1
                    && inst_block[ri] == b
                    && view.op(ri) == Op::Fmul
                    && Flags(view.flags[ri]).contains(Flags::CONTRACT)
                    && !skip_emit[ri]
                {
                    let pat = view.data[ri] as usize;
                    fusion[inst] = Fusion::Fmsub { a: view.extra[pat], b: view.extra[pat + 1], acc: lhs };
                    skip_emit[ri] = true;
                    continue;
                }
            }
            Op::Load | Op::Store => {
                let at = view.data[inst] as usize;
                let (ptr, off, access_ty) = if op == Op::Load {
                    (view.extra[at], i64::from(view.extra[at + 1] as i32), ty)
                } else {
                    let val = view.extra[at];
                    (view.extra[at + 1], i64::from(view.extra[at + 2] as i32), Ty(view.tys[val as usize]))
                };
                let (size, _) = mem_size_of(access_ty).unwrap();
                let pi = ptr as usize;
                let pop = view.op(pi);
                // Pattern A: `load`/`store` on `slot_addr(slot)` -> `[sp, #(slot_at[slot] + off)]`!
                if pop == Op::SlotAddr {
                    let s_off = i64::from(slot_at[view.data[pi] as usize]);
                    let total = s_off + off;
                    if fits_mem_off(size, total) {
                        fusion[inst] = Fusion::MemSpOff { off: total };
                        slot_addr_folded_uses[pi] += 1;
                        continue;
                    }
                }
                // Pattern B: `load`/`store` on single-use same-block `add`/`ptr_add(base, imm)` -> `[base, #(imm + off)]`!
                if use_count[pi] == 1 && inst_block[pi] == b && !skip_emit[pi] {
                    if pop == Op::PtrAdd {
                        let pat = view.data[pi] as usize;
                        let base = view.extra[pat];
                        let by = view.extra[pat + 1];
                        if let Some(cv) = const_u64[by as usize] {
                            let by_bits = Ty(view.tys[by as usize]).bits(64);
                            let delta = sign_extend_u64(cv, by_bits);
                            if let Some(total) = delta.checked_add(off) {
                                if fits_mem_off(size, total) {
                                    fusion[inst] = Fusion::MemBaseOff { base, off: total, imm_inst: by };
                                    skip_emit[pi] = true;
                                    continue;
                                }
                            }
                        }
                    } else if pop == Op::Add && Ty(view.tys[pi]).bits(64) == 64 && fusion[pi] == Fusion::None {
                        let pat = view.data[pi] as usize;
                        let a_lhs = view.extra[pat];
                        let a_rhs = view.extra[pat + 1];
                        let cand = if let Some(cv) = const_u64[a_rhs as usize] {
                            Some((a_lhs, a_rhs, cv as i64))
                        } else if let Some(cv) = const_u64[a_lhs as usize] {
                            Some((a_rhs, a_lhs, cv as i64))
                        } else {
                            None
                        };
                        if let Some((base, imm_inst, delta)) = cand {
                            if let Some(total) = delta.checked_add(off) {
                                if fits_mem_off(size, total) {
                                    fusion[inst] = Fusion::MemBaseOff { base, off: total, imm_inst };
                                    skip_emit[pi] = true;
                                    continue;
                                }
                            }
                        }
                    }
                }
            }
            Op::Br => {
                let at = view.data[inst] as usize;
                let cond_v = view.extra[at] as usize;
                if use_count[cond_v] == 1 && inst_block[cond_v] == b && !skip_emit[cond_v] {
                    if view.op(cond_v) == Op::Icmp {
                        let cat = view.data[cond_v] as usize;
                        if let Some(cond) = IntCond::from_word(view.extra[cat]) {
                            let a = view.extra[cat + 1];
                            let bv = view.extra[cat + 2];
                            let op_ty = Ty(view.tys[a as usize]);
                            fusion[inst] = Fusion::CmpBr { cond, a, b: bv, op_ty };
                            skip_emit[cond_v] = true;
                        }
                    } else if view.op(cond_v) == Op::Fcmp {
                        let cat = view.data[cond_v] as usize;
                        if let Some(fc) = FloatCond::from_word(view.extra[cat]).and_then(fcond_of) {
                            let a = view.extra[cat + 1];
                            let bv = view.extra[cat + 2];
                            if let Some(double) = is_fp_ty(Ty(view.tys[a as usize])) {
                                fusion[inst] = Fusion::FcmpBr { cond: fc, a, b: bv, double };
                                skip_emit[cond_v] = true;
                            }
                        }
                    }
                }
            }
            Op::Select => {
                let at = view.data[inst] as usize;
                let cond_v = view.extra[at] as usize;
                if use_count[cond_v] == 1 && inst_block[cond_v] == b && !skip_emit[cond_v] {
                    if view.op(cond_v) == Op::Icmp {
                        let cat = view.data[cond_v] as usize;
                        if let Some(cond) = IntCond::from_word(view.extra[cat]) {
                            let a = view.extra[cat + 1];
                            let bv = view.extra[cat + 2];
                            let op_ty = Ty(view.tys[a as usize]);
                            fusion[inst] = Fusion::CmpSelect { cond, a, b: bv, op_ty };
                            skip_emit[cond_v] = true;
                        }
                    } else if view.op(cond_v) == Op::Fcmp {
                        let cat = view.data[cond_v] as usize;
                        if let Some(fc) = FloatCond::from_word(view.extra[cat]).and_then(fcond_of) {
                            let a = view.extra[cat + 1];
                            let bv = view.extra[cat + 2];
                            if let Some(double) = is_fp_ty(Ty(view.tys[a as usize])) {
                                fusion[inst] = Fusion::FcmpSelect { cond: fc, a, b: bv, double };
                                skip_emit[cond_v] = true;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Any `slot_addr` whose EVERY use folded into `[sp, #off]` needs ZERO register & ZERO emission!
    for inst in 0..len {
        if view.op(inst) == Op::SlotAddr && use_count[inst] > 0 && slot_addr_folded_uses[inst] == use_count[inst] {
            skip_emit[inst] = true;
        }
    }

    // 2b. Count immediate-operand uses over ONLY the surviving (non-skipped) instructions & fusions,
    //     so constants consumed solely in immediate positions need ZERO register & ZERO emission!
    let mut imm_uses: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    for _ in 0..len {
        imm_uses.push(0);
    }
    for inst in 0..len {
        if skip_emit[inst] {
            continue;
        }
        let op = view.op(inst);
        let ty = Ty(view.tys[inst]);
        let bits = if ty == Ty::NONE || is_fp_ty(ty).is_some() { 0 } else { ty.bits(64) };
        match fusion[inst] {
            Fusion::AddLsl { imm_inst, .. }
            | Fusion::AddLsr { imm_inst, .. }
            | Fusion::SubLsl { imm_inst, .. }
            | Fusion::MemBaseOff { imm_inst, .. } => {
                imm_uses[imm_inst as usize] += 1;
            }
            Fusion::CmpBr { b: bv, op_ty, .. } | Fusion::CmpSelect { b: bv, op_ty, .. } => {
                if op_ty.bits(64) == 64 && const_u64[bv as usize].is_some_and(|v| v <= 4095) {
                    imm_uses[bv as usize] += 1;
                }
            }
            Fusion::Madd { .. }
            | Fusion::Msub { .. }
            | Fusion::Fmadd { .. }
            | Fusion::Fmsub { .. }
            | Fusion::FcmpBr { .. }
            | Fusion::FcmpSelect { .. }
            | Fusion::MemSpOff { .. } => {}
            Fusion::None => match op {
                Op::Add => {
                    let at = view.data[inst] as usize;
                    let lhs = view.extra[at] as usize;
                    let rhs = view.extra[at + 1] as usize;
                    if const_u64[rhs].is_some_and(|v| v <= 4095) {
                        imm_uses[rhs] += 1;
                    } else if const_u64[lhs].is_some_and(|v| v <= 4095) {
                        imm_uses[lhs] += 1;
                    }
                }
                Op::Sub => {
                    let at = view.data[inst] as usize;
                    let rhs = view.extra[at + 1] as usize;
                    if const_u64[rhs].is_some_and(|v| v <= 4095) {
                        imm_uses[rhs] += 1;
                    }
                }
                Op::PtrAdd => {
                    let at = view.data[inst] as usize;
                    let rhs = view.extra[at + 1] as usize;
                    if let Some(cv) = const_u64[rhs] {
                        let by_bits = Ty(view.tys[rhs]).bits(64);
                        let delta = sign_extend_u64(cv, by_bits);
                        if (-4095..=4095).contains(&delta) {
                            imm_uses[rhs] += 1;
                        }
                    }
                }
                Op::And => {
                    let at = view.data[inst] as usize;
                    let lhs = view.extra[at] as usize;
                    let rhs = view.extra[at + 1] as usize;
                    if const_u64[rhs].and_then(is_low_bitmask).is_some() {
                        imm_uses[rhs] += 1;
                    } else if const_u64[lhs].and_then(is_low_bitmask).is_some() {
                        imm_uses[lhs] += 1;
                    }
                }
                Op::Shl | Op::Lshr | Op::Ashr if bits == 64 => {
                    let at = view.data[inst] as usize;
                    let rhs = view.extra[at + 1] as usize;
                    if const_u64[rhs].is_some() {
                        imm_uses[rhs] += 1;
                    }
                }
                Op::Icmp => {
                    let at = view.data[inst] as usize;
                    let a = view.extra[at + 1] as usize;
                    let bv = view.extra[at + 2] as usize;
                    if Ty(view.tys[a]).bits(64) == 64 && const_u64[bv].is_some_and(|v| v <= 4095) {
                        imm_uses[bv] += 1;
                    }
                }
                _ => {}
            },
        }
    }
    for inst in 0..len {
        if const_u64[inst].is_some() && use_count[inst] > 0 && imm_uses[inst] == use_count[inst] {
            skip_emit[inst] = true;
        }
    }

    // 3. Spill-capable Chordal-SSA register allocation (GPR + scalar FP).
    let reg_alloc = regalloc::allocate(&view, &skip_emit, max_spill_slots, alloc).map_err(|_| TierError::Refused {
        tier: TIER,
        inst: None,
        what: "spill slots exceeded Sota fast frame budget",
    })?;

    let used_bytes = u64::from(spill_base) + u64::from(reg_alloc.num_spill_slots) * 8;
    let frame_size_u64 = used_bytes.next_multiple_of(16);
    if frame_size_u64 + (most_align - 16) >= FRAME_LIMIT {
        return Err(TierError::Refused { tier: TIER, inst: None, what: "stack frame >= 16 MiB" });
    }
    let frame_size = frame_size_u64 as u32;

    // Helper closures for register / spill access.
    let load_gpr = |words: &mut Vec<u32, Alloc<'a>>, v: u32, scratch: Reg| -> Reg {
        match reg_alloc.loc_of[v as usize] {
            AllocLoc::Gpr(r) => u32::from(r),
            AllocLoc::GprSpill(slot) => {
                emit_ld_spill_gpr(words, scratch, spill_base, slot);
                scratch
            }
            AllocLoc::None => {
                if let Some(cval) = const_u64[v as usize] {
                    emit_const_u64(words, scratch, cval);
                    scratch
                } else {
                    panic!("Sota internal error: use of unallocated GPR value {v}");
                }
            }
            other => panic!("Sota internal error: expected GPR for value {v}, got {other:?}"),
        }
    };

    let load_fp = |words: &mut Vec<u32, Alloc<'a>>, v: u32, scratch: Reg| -> Reg {
        match reg_alloc.loc_of[v as usize] {
            AllocLoc::Fp(r) => u32::from(r),
            AllocLoc::FpSpill(slot) => {
                emit_ld_spill_fp(words, scratch, spill_base, slot);
                scratch
            }
            other => panic!("Sota internal error: expected FP for value {v}, got {other:?}"),
        }
    };

    let dst_gpr = |inst: usize| -> (Reg, Option<u32>) {
        match reg_alloc.loc_of[inst] {
            AllocLoc::Gpr(r) => (u32::from(r), None),
            AllocLoc::GprSpill(slot) => (X16, Some(slot)),
            AllocLoc::None => (X16, None),
            other => panic!("Sota internal error: expected GPR dest for inst {inst}, got {other:?}"),
        }
    };

    let dst_fp = |inst: usize| -> (Reg, Option<u32>) {
        match reg_alloc.loc_of[inst] {
            AllocLoc::Fp(r) => (u32::from(r), None),
            AllocLoc::FpSpill(slot) => (V30, Some(slot)),
            AllocLoc::None => (V30, None),
            other => panic!("Sota internal error: expected FP dest for inst {inst}, got {other:?}"),
        }
    };

    // 4. Emit AArch64 words!
    let mut words: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(len * 2 + 16, alloc);
    let mut patches: Vec<BranchPatch, Alloc<'a>> = Vec::new_in(alloc);
    let mut block_word_off: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        block_word_off.push(0);
    }
    let mut gpr_moves: Vec<(GprCopyLoc, GprCopyLoc), Alloc<'a>> = Vec::with_capacity_in(16, alloc);
    let mut fp_moves: Vec<(FpCopyLoc, FpCopyLoc), Alloc<'a>> = Vec::with_capacity_in(16, alloc);
    let mut const_edge_writes: Vec<(GprCopyLoc, u64), Alloc<'a>> = Vec::with_capacity_in(8, alloc);
    let mut data_relro_off: Vec<Option<u32>, Alloc<'a>> = Vec::with_capacity_in(view.datas.len(), alloc);
    for _ in 0..view.datas.len() {
        data_relro_off.push(None);
    }
    let mut out = Emitted::new_in(alloc);

    // Frameless leaf prologue when no calls, callee-saved regs, stack slots, or spill slots!
    let x_pairs = reg_alloc.x_pairs;
    let d_pairs = reg_alloc.d_pairs;
    let cs_bytes = (x_pairs.count_ones() + d_pairs.count_ones()) * 16;
    let needs_frame = reg_alloc.has_calls || x_pairs != 0 || d_pairs != 0 || frame_size > 0 || most_align > 16;

    if needs_frame {
        words.push(enc::stp_pre(FP, LR, SP, -16));
        words.push(enc::add_imm(FP, SP, 0, false)); // mov x29, sp
        // Save callee-saved X pairs in exact ARM64 unwind order (`stp x(20+2k), x(19+2k), [sp, #-16]!`).
        for k in 0..5u32 {
            if x_pairs & (1 << k) != 0 {
                let lo = 19 + 2 * k;
                let hi = lo + 1;
                words.push(enc::stp_pre(hi, lo, SP, -16));
            }
        }
        // Save callee-saved D pairs in exact ARM64 unwind order (`stp d(9+2k), d(8+2k), [sp, #-16]!`).
        for k in 0..4u32 {
            if d_pairs & (1 << k) != 0 {
                let lo = 8 + 2 * k;
                let hi = lo + 1;
                words.push(enc::stp_pre_d(hi, lo, SP, -16));
            }
        }
        let unwind_frame = blaze_code::unwind::Frame { x_pairs, d_pairs };
        out.frame = unwind_frame.word().unwrap_or(0);
        out.unwind_frame = Some(unwind_frame);

        if frame_size > 0 {
            let drop = 16u64 + u64::from(frame_size) + (most_align - 16);
            if drop <= u64::from(PROBE_STEP) {
                let hi = frame_size >> 12;
                let lo = frame_size & 0xfff;
                if hi != 0 {
                    words.push(enc::sub_imm(SP, SP, hi, true));
                }
                if lo != 0 {
                    words.push(enc::sub_imm(SP, SP, lo, false));
                }
            } else {
                // Inline stack probe loop so frames > 4 KiB never step over a guard page.
                words.push(enc::movz(X17, frame_size & 0xffff, 0));
                if frame_size >> 16 != 0 {
                    words.push(enc::movk(X17, frame_size >> 16, 1));
                }
                let step = PROBE_STEP >> 12;
                if frame_size >= PROBE_STEP {
                    let probe_loop = words.len();
                    words.push(enc::sub_imm(SP, SP, step, true));
                    words.push(enc::str(Size::X, ZR, SP, 0));
                    words.push(enc::sub_imm(X17, X17, step, true));
                    words.push(enc_probe::cmp_imm_lsl12(X17, step));
                    let delta = probe_loop as i32 - words.len() as i32;
                    words.push(enc::b_cond(Cond::Hs, delta));
                }
                words.push(enc_probe::sub_ext_uxtx(SP, SP, X17));
                if most_align > 16 {
                    words.push(enc::str(Size::X, ZR, SP, 0));
                }
            }
        }
        if most_align > 16 {
            let log2 = most_align.trailing_zeros();
            words.push(enc::add_imm(X16, SP, 0, false));
            words.push(enc::lsr_imm(X16, X16, log2));
            words.push(enc::lsl_imm(X16, X16, log2));
            words.push(enc::add_imm(SP, X16, 0, false));
        }
    }

    // Marshal entry parameters from `X0..X7` / `V0..V7` into their allocated regs/spill slots.
    let entry_params = view.params(Block(0));
    gpr_moves.clear();
    fp_moves.clear();
    let mut gpr_arg_idx = 0usize;
    let mut fp_arg_idx = 0usize;
    for p in entry_params.clone() {
        let ty = Ty(view.tys[p]);
        if is_fp_ty(ty).is_some() {
            let src = FP_ARG_REGS[fp_arg_idx];
            fp_arg_idx += 1;
            match reg_alloc.loc_of[p] {
                AllocLoc::Fp(dst) => fp_moves.push((FpCopyLoc::Reg(u32::from(dst)), FpCopyLoc::Reg(src))),
                AllocLoc::FpSpill(slot) => emit_st_spill_fp(&mut words, src, spill_base, slot),
                AllocLoc::None => {}
                other => panic!("Sota internal error: FP entry param got {other:?}"),
            }
        } else {
            let src = ARG_REGS[gpr_arg_idx];
            gpr_arg_idx += 1;
            let bits = ty.bits(64);
            match reg_alloc.loc_of[p] {
                AllocLoc::Gpr(dst) => gpr_moves.push((GprCopyLoc::Reg(u32::from(dst)), GprCopyLoc::Reg(src))),
                AllocLoc::GprSpill(slot) => {
                    if bits < 64 {
                        words.push(enc::zext(X16, src, bits));
                        emit_st_spill_gpr(&mut words, X16, spill_base, slot);
                    } else {
                        emit_st_spill_gpr(&mut words, src, spill_base, slot);
                    }
                }
                AllocLoc::None => {}
                other => panic!("Sota internal error: GPR entry param got {other:?}"),
            }
        }
    }
    emit_parallel_gpr_moves(&mut words, &mut gpr_moves, spill_base);
    emit_parallel_fp_moves(&mut words, &mut fp_moves, spill_base);
    // Narrow sub-64-bit register-resident GPR entry params to preserve zero-extension invariant.
    for p in entry_params {
        if let AllocLoc::Gpr(dst) = reg_alloc.loc_of[p] {
            let ty = Ty(view.tys[p]);
            if is_fp_ty(ty).is_none() {
                emit_narrow(&mut words, u32::from(dst), ty.bits(64));
            }
        }
    }

    // Helper: emit signed/unsigned integer compare of `a` and `b` setting NZCV flags.
    let emit_cmp = |words: &mut Vec<u32, Alloc<'a>>, cond: IntCond, a: u32, bv: u32, op_ty: Ty| {
        let bits = op_ty.bits(64);
        let ra = load_gpr(words, a, X16);
        let signed = matches!(cond, IntCond::Slt | IntCond::Sle | IntCond::Sgt | IntCond::Sge);
        if bits == 64 && const_u64[bv as usize].is_some_and(|v| v <= 4095) {
            let imm = const_u64[bv as usize].unwrap() as u32;
            words.push(enc::cmp_imm(ra, imm));
        } else if signed && bits < 64 {
            let rb = load_gpr(words, bv, X17);
            words.push(enc::sext(X16, ra, bits));
            words.push(enc::sext(X17, rb, bits));
            words.push(enc::cmp(X16, X17));
        } else {
            let rb = load_gpr(words, bv, X17);
            words.push(enc::cmp(ra, rb));
        }
    };

    // Helper: emit parallel moves for a branch/jump/switch target's block arguments.
    let emit_edge_moves = |words: &mut Vec<u32, Alloc<'a>>,
                           gpr_moves: &mut Vec<(GprCopyLoc, GprCopyLoc), Alloc<'a>>,
                           fp_moves: &mut Vec<(FpCopyLoc, FpCopyLoc), Alloc<'a>>,
                           const_writes: &mut Vec<(GprCopyLoc, u64), Alloc<'a>>,
                           target_block: u32,
                           args_start: usize,
                           args_len: usize| {
        let target_params = view.params(Block(target_block));
        gpr_moves.clear();
        fp_moves.clear();
        const_writes.clear();
        for k in 0..args_len {
            let param_inst = target_params.start + k;
            let dst_loc = reg_alloc.loc_of[param_inst];
            if dst_loc == AllocLoc::None {
                continue;
            }
            let src_v = view.extra[args_start + k];
            match dst_loc {
                AllocLoc::Gpr(d) => {
                    let dst = GprCopyLoc::Reg(u32::from(d));
                    match reg_alloc.loc_of[src_v as usize] {
                        AllocLoc::Gpr(s) => gpr_moves.push((dst, GprCopyLoc::Reg(u32::from(s)))),
                        AllocLoc::GprSpill(s) => gpr_moves.push((dst, GprCopyLoc::Spill(s))),
                        AllocLoc::None => {
                            let cval = const_u64[src_v as usize].expect("unallocated non-const edge arg");
                            const_writes.push((dst, cval));
                        }
                        other => panic!("Sota internal error: GPR edge arg got {other:?}"),
                    }
                }
                AllocLoc::GprSpill(d) => {
                    let dst = GprCopyLoc::Spill(d);
                    match reg_alloc.loc_of[src_v as usize] {
                        AllocLoc::Gpr(s) => gpr_moves.push((dst, GprCopyLoc::Reg(u32::from(s)))),
                        AllocLoc::GprSpill(s) => gpr_moves.push((dst, GprCopyLoc::Spill(s))),
                        AllocLoc::None => {
                            let cval = const_u64[src_v as usize].expect("unallocated non-const edge arg");
                            const_writes.push((dst, cval));
                        }
                        other => panic!("Sota internal error: GPR spill edge arg got {other:?}"),
                    }
                }
                AllocLoc::Fp(d) => {
                    let dst = FpCopyLoc::Reg(u32::from(d));
                    match reg_alloc.loc_of[src_v as usize] {
                        AllocLoc::Fp(s) => fp_moves.push((dst, FpCopyLoc::Reg(u32::from(s)))),
                        AllocLoc::FpSpill(s) => fp_moves.push((dst, FpCopyLoc::Spill(s))),
                        other => panic!("Sota internal error: FP edge arg got {other:?}"),
                    }
                }
                AllocLoc::FpSpill(d) => {
                    let dst = FpCopyLoc::Spill(d);
                    match reg_alloc.loc_of[src_v as usize] {
                        AllocLoc::Fp(s) => fp_moves.push((dst, FpCopyLoc::Reg(u32::from(s)))),
                        AllocLoc::FpSpill(s) => fp_moves.push((dst, FpCopyLoc::Spill(s))),
                        other => panic!("Sota internal error: FP spill edge arg got {other:?}"),
                    }
                }
                AllocLoc::None => {}
            }
        }
        emit_parallel_gpr_moves(words, gpr_moves, spill_base);
        emit_parallel_fp_moves(words, fp_moves, spill_base);
        // Materialize skipped constant edge arguments AFTER parallel moves so they never clobber any source!
        for &(dst, cval) in const_writes.iter() {
            match dst {
                GprCopyLoc::Reg(r) => emit_const_u64(words, r, cval),
                GprCopyLoc::Spill(s) => {
                    emit_const_u64(words, X16, cval);
                    emit_st_spill_gpr(words, X16, spill_base, s);
                }
            }
        }
    };

    let func_sig = view.sig(SigId(view.head.sig));

    // Emit each block in layout order!
    for b in 0..blocks {
        block_word_off[b] = words.len();
        let range = view.block(Block(b as u32));
        for inst in range {
            if skip_emit[inst] {
                continue;
            }
            let op = view.op(inst);
            let ty = Ty(view.tys[inst]);
            let fp_ty = is_fp_ty(ty);
            let bits = if ty == Ty::NONE || fp_ty.is_some() { 0 } else { ty.bits(64) };
            let loc = reg_alloc.loc_of[inst];

            // Skip dead pure non-side-effecting instructions (never skip terminators, calls, loads, stores, traps, div/rem).
            let has_side_effect = op.is_terminator()
                || matches!(
                    op,
                    Op::Call
                        | Op::CallIndirect
                        | Op::Load
                        | Op::Store
                        | Op::Trap
                        | Op::Udiv
                        | Op::Sdiv
                        | Op::Urem
                        | Op::Srem
                );
            if !has_side_effect && loc == AllocLoc::None {
                continue;
            }

            match op {
                Op::Param | Op::SlotLive | Op::SlotDead | Op::Assume => {}
                Op::Null => {
                    let (rd, sp) = dst_gpr(inst);
                    words.push(enc::movz(rd, 0, 0));
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Uninit => {
                    if fp_ty.is_some() {
                        let (vd, sp) = dst_fp(inst);
                        words.push(enc::fmov_to_d(vd, ZR));
                        if let Some(s) = sp {
                            emit_st_spill_fp(&mut words, vd, spill_base, s);
                        }
                    } else {
                        let (rd, sp) = dst_gpr(inst);
                        words.push(enc::movz(rd, 0, 0));
                        if let Some(s) = sp {
                            emit_st_spill_gpr(&mut words, rd, spill_base, s);
                        }
                    }
                }
                Op::Iconst | Op::IconstWide => {
                    let (rd, sp) = dst_gpr(inst);
                    let val = const_u64[inst].unwrap_or(0);
                    emit_const_u64(&mut words, rd, val);
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Fconst => {
                    let (vd, sp) = dst_fp(inst);
                    let fbits = view.data[inst];
                    if fbits == 0 {
                        words.push(enc::fmov_to_s(vd, ZR));
                    } else {
                        emit_const_u64(&mut words, X16, u64::from(fbits));
                        words.push(enc::fmov_to_s(vd, X16));
                    }
                    if let Some(s) = sp {
                        emit_st_spill_fp(&mut words, vd, spill_base, s);
                    }
                }
                Op::FconstWide => {
                    let (vd, sp) = dst_fp(inst);
                    let at = view.data[inst] as usize;
                    let fbits = u64::from(view.extra[at]) | (u64::from(view.extra[at + 1]) << 32);
                    if fbits == 0 {
                        words.push(enc::fmov_to_d(vd, ZR));
                    } else {
                        emit_const_u64(&mut words, X16, fbits);
                        words.push(enc::fmov_to_d(vd, X16));
                    }
                    if let Some(s) = sp {
                        emit_st_spill_fp(&mut words, vd, spill_base, s);
                    }
                }
                Op::Freeze => {
                    if let Some(double) = fp_ty {
                        let (vd, sp) = dst_fp(inst);
                        let vs = load_fp(&mut words, view.data[inst], V30);
                        if vd != vs {
                            words.push(enc::fmov_reg(double, vd, vs));
                        }
                        if let Some(s) = sp {
                            emit_st_spill_fp(&mut words, vd, spill_base, s);
                        }
                    } else {
                        let (rd, sp) = dst_gpr(inst);
                        let src = load_gpr(&mut words, view.data[inst], X16);
                        if rd != src {
                            words.push(enc::mov(rd, src));
                        }
                        if let Some(s) = sp {
                            emit_st_spill_gpr(&mut words, rd, spill_base, s);
                        }
                    }
                }
                Op::PtrAddr | Op::Expose | Op::FromExposed => {
                    let (rd, sp) = dst_gpr(inst);
                    let src = load_gpr(&mut words, view.data[inst], X16);
                    if rd != src {
                        words.push(enc::mov(rd, src));
                    }
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::PtrWithAddr => {
                    let at = view.data[inst] as usize;
                    let addr = view.extra[at + 1];
                    let (rd, sp) = dst_gpr(inst);
                    let src = load_gpr(&mut words, addr, X16);
                    if rd != src {
                        words.push(enc::mov(rd, src));
                    }
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::SlotAddr => {
                    let (rd, sp) = dst_gpr(inst);
                    let off = slot_at[view.data[inst] as usize];
                    emit_sp_addr(&mut words, rd, off);
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::PtrAdd => {
                    let at = view.data[inst] as usize;
                    let ptr = view.extra[at];
                    let by = view.extra[at + 1];
                    let (rd, sp) = dst_gpr(inst);
                    let rptr = load_gpr(&mut words, ptr, X16);
                    if let Some(cv) = const_u64[by as usize] {
                        let by_bits = Ty(view.tys[by as usize]).bits(64);
                        let delta = sign_extend_u64(cv, by_bits);
                        if (0..=4095).contains(&delta) {
                            words.push(enc::add_imm(rd, rptr, delta as u32, false));
                        } else if (-4095..0).contains(&delta) {
                            words.push(enc::sub_imm(rd, rptr, (-delta) as u32, false));
                        } else {
                            emit_const_u64(&mut words, X17, delta as u64);
                            words.push(enc::add(rd, rptr, X17));
                        }
                    } else {
                        let by_bits = Ty(view.tys[by as usize]).bits(64);
                        let rby = load_gpr(&mut words, by, X17);
                        if by_bits < 64 {
                            words.push(enc::sext(X17, rby, by_bits));
                            words.push(enc::add(rd, rptr, X17));
                        } else {
                            words.push(enc::add(rd, rptr, rby));
                        }
                    }
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Load => {
                    let at = view.data[inst] as usize;
                    let (size, is_fp) = mem_size_of(ty).unwrap();
                    let (dst, sp) = if is_fp { dst_fp(inst) } else { dst_gpr(inst) };
                    match fusion[inst] {
                        Fusion::MemSpOff { off } => {
                            emit_mem_imm(&mut words, true, is_fp, size, dst, SP, off);
                        }
                        Fusion::MemBaseOff { base, off, .. } => {
                            let rbase = load_gpr(&mut words, base, X17);
                            emit_mem_imm(&mut words, true, is_fp, size, dst, rbase, off);
                        }
                        _ => {
                            let ptr = view.extra[at];
                            let off = i64::from(view.extra[at + 1] as i32);
                            if fits_mem_off(size, off) {
                                let rbase = load_gpr(&mut words, ptr, X17);
                                emit_mem_imm(&mut words, true, is_fp, size, dst, rbase, off);
                            } else {
                                let rptr = load_gpr(&mut words, ptr, X16);
                                emit_const_u64(&mut words, X17, off as u64);
                                words.push(enc::add(X17, rptr, X17));
                                emit_mem_imm(&mut words, true, is_fp, size, dst, X17, 0);
                            }
                        }
                    }
                    if let Some(s) = sp {
                        if is_fp {
                            emit_st_spill_fp(&mut words, dst, spill_base, s);
                        } else {
                            emit_st_spill_gpr(&mut words, dst, spill_base, s);
                        }
                    }
                }
                Op::Store => {
                    let at = view.data[inst] as usize;
                    let val = view.extra[at];
                    let val_ty = Ty(view.tys[val as usize]);
                    let (size, is_fp) = mem_size_of(val_ty).unwrap();
                    match fusion[inst] {
                        Fusion::MemSpOff { off } => {
                            let rval = if is_fp {
                                load_fp(&mut words, val, V30)
                            } else {
                                load_gpr(&mut words, val, X16)
                            };
                            emit_mem_imm(&mut words, false, is_fp, size, rval, SP, off);
                        }
                        Fusion::MemBaseOff { base, off, .. } => {
                            let rval = if is_fp {
                                load_fp(&mut words, val, V30)
                            } else {
                                load_gpr(&mut words, val, X16)
                            };
                            let rbase = load_gpr(&mut words, base, X17);
                            emit_mem_imm(&mut words, false, is_fp, size, rval, rbase, off);
                        }
                        _ => {
                            let ptr = view.extra[at + 1];
                            let off = i64::from(view.extra[at + 2] as i32);
                            if fits_mem_off(size, off) {
                                let rval = if is_fp {
                                    load_fp(&mut words, val, V30)
                                } else {
                                    load_gpr(&mut words, val, X16)
                                };
                                let rbase = load_gpr(&mut words, ptr, X17);
                                emit_mem_imm(&mut words, false, is_fp, size, rval, rbase, off);
                            } else {
                                // Compute effective address in X17 first so X16 stays free for a spilled value!
                                let rptr = load_gpr(&mut words, ptr, X16);
                                emit_const_u64(&mut words, X17, off as u64);
                                words.push(enc::add(X17, rptr, X17));
                                let rval = if is_fp {
                                    load_fp(&mut words, val, V30)
                                } else {
                                    load_gpr(&mut words, val, X16)
                                };
                                emit_mem_imm(&mut words, false, is_fp, size, rval, X17, 0);
                            }
                        }
                    }
                }
                Op::DataAddr => {
                    let (rd, sp) = dst_gpr(inst);
                    let rec = view.reference(RefId(view.data[inst]));
                    match RefKind::from_word(rec.kind) {
                        Some(RefKind::Static) => {
                            let target = resolve_outside(names, Outside::Static(rec.place), Space::Static, true, &mut out, inst)?;
                            let adrp_at = (words.len() * 4) as u32;
                            words.push(enc::adrp(rd, 0));
                            words.push(enc::ldr(Size::X, rd, rd, 0));
                            out.relocs.push(Reloc { at: adrp_at, in_relro: false, kind: RelocKind::GotPage21, target, addend: 0 });
                            out.relocs.push(Reloc { at: adrp_at + 4, in_relro: false, kind: RelocKind::GotPageOff12, target, addend: 0 });
                        }
                        Some(RefKind::Vtable) => {
                            let target = resolve_outside(names, Outside::Vtable(rec.place), Space::Vtable, false, &mut out, inst)?;
                            let adrp_at = (words.len() * 4) as u32;
                            words.push(enc::adrp(rd, 0));
                            words.push(enc::ldr(Size::X, rd, rd, 0));
                            out.relocs.push(Reloc { at: adrp_at, in_relro: false, kind: RelocKind::GotPage21, target, addend: 0 });
                            out.relocs.push(Reloc { at: adrp_at + 4, in_relro: false, kind: RelocKind::GotPageOff12, target, addend: 0 });
                        }
                        Some(RefKind::Data) => {
                            let row = rec.place;
                            let off = if let Some(off) = data_relro_off[row as usize] {
                                off
                            } else {
                                let data = view.datas[row as usize];
                                let align = 8usize.max(1 << data.align);
                                out.relro_align = out.relro_align.max(data.align as u8);
                                while out.relro.len() % align != 0 {
                                    out.relro.push(0);
                                }
                                let off = out.relro.len() as u32;
                                let bytes = view.data_bytes_of(DataId(row));
                                if bytes.is_empty() {
                                    // Keep `addend < relro.len()` valid for `CodeRecord::seal`.
                                    out.relro.extend_from_slice(&[0; 8]);
                                } else {
                                    out.relro.extend_from_slice(bytes);
                                }
                                data_relro_off[row as usize] = Some(off);
                                off
                            };
                            let adrp_at = (words.len() * 4) as u32;
                            words.push(enc::adrp(rd, 0));
                            words.push(enc::add_imm(rd, rd, 0, false));
                            out.relocs.push(Reloc {
                                at: adrp_at,
                                in_relro: false,
                                kind: RelocKind::Page21,
                                target: OWN_RELRO,
                                addend: i64::from(off),
                            });
                            out.relocs.push(Reloc {
                                at: adrp_at + 4,
                                in_relro: false,
                                kind: RelocKind::PageOff12 { scale: 0 },
                                target: OWN_RELRO,
                                addend: i64::from(off),
                            });
                        }
                        _ => {
                            return Err(TierError::Refused { tier: TIER, inst: Some(inst as u32), what: "unsupported data_addr" });
                        }
                    }
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::FuncAddr => {
                    let (rd, sp) = dst_gpr(inst);
                    let rec = view.reference(RefId(view.data[inst]));
                    let target = resolve_outside(names, Outside::Func(rec.place), Space::Func, true, &mut out, inst)?;
                    let adrp_at = (words.len() * 4) as u32;
                    words.push(enc::adrp(rd, 0));
                    words.push(enc::ldr(Size::X, rd, rd, 0));
                    out.relocs.push(Reloc { at: adrp_at, in_relro: false, kind: RelocKind::GotPage21, target, addend: 0 });
                    out.relocs.push(Reloc { at: adrp_at + 4, in_relro: false, kind: RelocKind::GotPageOff12, target, addend: 0 });
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Neg => {
                    let (rd, sp) = dst_gpr(inst);
                    let src = load_gpr(&mut words, view.data[inst], X16);
                    words.push(enc::neg(rd, src));
                    emit_narrow(&mut words, rd, bits);
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Not => {
                    let (rd, sp) = dst_gpr(inst);
                    let src = load_gpr(&mut words, view.data[inst], X16);
                    words.push(enc::mvn(rd, src));
                    emit_narrow(&mut words, rd, bits);
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Add => {
                    let (rd, sp) = dst_gpr(inst);
                    match fusion[inst] {
                        Fusion::Madd { a, b: mb, acc } => {
                            let spill_cnt = [a, mb, acc]
                                .iter()
                                .filter(|&&v| !matches!(reg_alloc.loc_of[v as usize], AllocLoc::Gpr(_)))
                                .count();
                            if spill_cnt <= 2 {
                                let mut next_s = X16;
                                let mut grab = |words: &mut Vec<u32, Alloc<'a>>, v: u32| -> Reg {
                                    match reg_alloc.loc_of[v as usize] {
                                        AllocLoc::Gpr(r) => u32::from(r),
                                        _ => {
                                            let s = next_s;
                                            next_s = X17;
                                            load_gpr(words, v, s)
                                        }
                                    }
                                };
                                let ra = grab(&mut words, a);
                                let rb = grab(&mut words, mb);
                                let racc = grab(&mut words, acc);
                                words.push(enc::madd(rd, ra, rb, racc));
                            } else {
                                let ra = load_gpr(&mut words, a, X16);
                                let rb = load_gpr(&mut words, mb, X17);
                                words.push(enc::mul(X16, ra, rb));
                                let racc = load_gpr(&mut words, acc, X17);
                                words.push(enc::add(rd, X16, racc));
                            }
                        }
                        Fusion::AddLsl { a, b: sb, shift, .. } => {
                            let ra = load_gpr(&mut words, a, X16);
                            let rb = load_gpr(&mut words, sb, X17);
                            words.push(enc::add_lsl(rd, ra, rb, shift));
                        }
                        Fusion::AddLsr { a, b: sb, shift, .. } => {
                            let ra = load_gpr(&mut words, a, X16);
                            let rb = load_gpr(&mut words, sb, X17);
                            words.push(enc::add_lsr(rd, ra, rb, shift));
                        }
                        _ => {
                            let at = view.data[inst] as usize;
                            let lhs = view.extra[at];
                            let rhs = view.extra[at + 1];
                            if let Some(imm) = const_u64[rhs as usize].filter(|&v| v <= 4095) {
                                let rl = load_gpr(&mut words, lhs, X16);
                                words.push(enc::add_imm(rd, rl, imm as u32, false));
                            } else if let Some(imm) = const_u64[lhs as usize].filter(|&v| v <= 4095) {
                                let rr = load_gpr(&mut words, rhs, X16);
                                words.push(enc::add_imm(rd, rr, imm as u32, false));
                            } else {
                                let rl = load_gpr(&mut words, lhs, X16);
                                let rr = load_gpr(&mut words, rhs, X17);
                                words.push(enc::add(rd, rl, rr));
                            }
                            emit_narrow(&mut words, rd, bits);
                        }
                    }
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Sub => {
                    let (rd, sp) = dst_gpr(inst);
                    match fusion[inst] {
                        Fusion::Msub { a, b: mb, acc } => {
                            let spill_cnt = [a, mb, acc]
                                .iter()
                                .filter(|&&v| !matches!(reg_alloc.loc_of[v as usize], AllocLoc::Gpr(_)))
                                .count();
                            if spill_cnt <= 2 {
                                let mut next_s = X16;
                                let mut grab = |words: &mut Vec<u32, Alloc<'a>>, v: u32| -> Reg {
                                    match reg_alloc.loc_of[v as usize] {
                                        AllocLoc::Gpr(r) => u32::from(r),
                                        _ => {
                                            let s = next_s;
                                            next_s = X17;
                                            load_gpr(words, v, s)
                                        }
                                    }
                                };
                                let ra = grab(&mut words, a);
                                let rb = grab(&mut words, mb);
                                let racc = grab(&mut words, acc);
                                words.push(enc::msub(rd, ra, rb, racc));
                            } else {
                                let ra = load_gpr(&mut words, a, X16);
                                let rb = load_gpr(&mut words, mb, X17);
                                words.push(enc::mul(X16, ra, rb));
                                let racc = load_gpr(&mut words, acc, X17);
                                words.push(enc::sub(rd, racc, X16));
                            }
                        }
                        Fusion::SubLsl { a, b: sb, shift, .. } => {
                            let ra = load_gpr(&mut words, a, X16);
                            let rb = load_gpr(&mut words, sb, X17);
                            words.push(enc::sub_lsl(rd, ra, rb, shift));
                        }
                        _ => {
                            let at = view.data[inst] as usize;
                            let lhs = view.extra[at];
                            let rhs = view.extra[at + 1];
                            if let Some(imm) = const_u64[rhs as usize].filter(|&v| v <= 4095) {
                                let rl = load_gpr(&mut words, lhs, X16);
                                words.push(enc::sub_imm(rd, rl, imm as u32, false));
                            } else {
                                let rl = load_gpr(&mut words, lhs, X16);
                                let rr = load_gpr(&mut words, rhs, X17);
                                words.push(enc::sub(rd, rl, rr));
                            }
                            emit_narrow(&mut words, rd, bits);
                        }
                    }
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Mul => {
                    let at = view.data[inst] as usize;
                    let (rd, sp) = dst_gpr(inst);
                    let rl = load_gpr(&mut words, view.extra[at], X16);
                    let rr = load_gpr(&mut words, view.extra[at + 1], X17);
                    words.push(enc::mul(rd, rl, rr));
                    emit_narrow(&mut words, rd, bits);
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::And => {
                    let at = view.data[inst] as usize;
                    let lhs = view.extra[at];
                    let rhs = view.extra[at + 1];
                    let (rd, sp) = dst_gpr(inst);
                    if let Some(k) = const_u64[rhs as usize].and_then(is_low_bitmask) {
                        let rl = load_gpr(&mut words, lhs, X16);
                        words.push(enc::ubfm(rd, rl, 0, k - 1));
                    } else if let Some(k) = const_u64[lhs as usize].and_then(is_low_bitmask) {
                        let rr = load_gpr(&mut words, rhs, X16);
                        words.push(enc::ubfm(rd, rr, 0, k - 1));
                    } else {
                        let rl = load_gpr(&mut words, lhs, X16);
                        let rr = load_gpr(&mut words, rhs, X17);
                        words.push(enc::and(rd, rl, rr));
                    }
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Or => {
                    let at = view.data[inst] as usize;
                    let (rd, sp) = dst_gpr(inst);
                    let rl = load_gpr(&mut words, view.extra[at], X16);
                    let rr = load_gpr(&mut words, view.extra[at + 1], X17);
                    words.push(enc::orr(rd, rl, rr));
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Xor => {
                    let at = view.data[inst] as usize;
                    let (rd, sp) = dst_gpr(inst);
                    let rl = load_gpr(&mut words, view.extra[at], X16);
                    let rr = load_gpr(&mut words, view.extra[at + 1], X17);
                    words.push(enc::eor(rd, rl, rr));
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Shl => {
                    let at = view.data[inst] as usize;
                    let lhs = view.extra[at];
                    let rhs = view.extra[at + 1];
                    let (rd, sp) = dst_gpr(inst);
                    let rl = load_gpr(&mut words, lhs, X16);
                    if bits == 64 {
                        if let Some(k) = const_u64[rhs as usize] {
                            let sh = (k & 63) as u32;
                            if sh == 0 {
                                if rd != rl {
                                    words.push(enc::mov(rd, rl));
                                }
                            } else {
                                words.push(enc::lsl_imm(rd, rl, sh));
                            }
                        } else {
                            let rr = load_gpr(&mut words, rhs, X17);
                            words.push(enc::lslv(rd, rl, rr));
                        }
                    } else {
                        let mask_bits = bits.trailing_zeros();
                        if mask_bits == 0 {
                            if rd != rl {
                                words.push(enc::mov(rd, rl));
                            }
                        } else {
                            let rr = load_gpr(&mut words, rhs, X17);
                            words.push(enc::ubfm(X17, rr, 0, mask_bits - 1));
                            words.push(enc::lslv(rd, rl, X17));
                            emit_narrow(&mut words, rd, bits);
                        }
                    }
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Lshr => {
                    let at = view.data[inst] as usize;
                    let lhs = view.extra[at];
                    let rhs = view.extra[at + 1];
                    let (rd, sp) = dst_gpr(inst);
                    let rl = load_gpr(&mut words, lhs, X16);
                    if bits == 64 {
                        if let Some(k) = const_u64[rhs as usize] {
                            let sh = (k & 63) as u32;
                            if sh == 0 {
                                if rd != rl {
                                    words.push(enc::mov(rd, rl));
                                }
                            } else {
                                words.push(enc::lsr_imm(rd, rl, sh));
                            }
                        } else {
                            let rr = load_gpr(&mut words, rhs, X17);
                            words.push(enc::lsrv(rd, rl, rr));
                        }
                    } else {
                        let mask_bits = bits.trailing_zeros();
                        if mask_bits == 0 {
                            if rd != rl {
                                words.push(enc::mov(rd, rl));
                            }
                        } else {
                            let rr = load_gpr(&mut words, rhs, X17);
                            words.push(enc::ubfm(X17, rr, 0, mask_bits - 1));
                            words.push(enc::lsrv(rd, rl, X17));
                            emit_narrow(&mut words, rd, bits);
                        }
                    }
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Ashr => {
                    let at = view.data[inst] as usize;
                    let lhs = view.extra[at];
                    let rhs = view.extra[at + 1];
                    let (rd, sp) = dst_gpr(inst);
                    let rl = load_gpr(&mut words, lhs, X16);
                    if bits == 64 {
                        if let Some(k) = const_u64[rhs as usize] {
                            let sh = (k & 63) as u32;
                            if sh == 0 {
                                if rd != rl {
                                    words.push(enc::mov(rd, rl));
                                }
                            } else {
                                words.push(enc::sbfm(rd, rl, sh, 63));
                            }
                        } else {
                            let rr = load_gpr(&mut words, rhs, X17);
                            words.push(enc::asrv(rd, rl, rr));
                        }
                    } else {
                        let mask_bits = bits.trailing_zeros();
                        if mask_bits == 0 {
                            if rd != rl {
                                words.push(enc::mov(rd, rl));
                            }
                        } else {
                            let rr = load_gpr(&mut words, rhs, X17);
                            words.push(enc::sext(X16, rl, bits));
                            words.push(enc::ubfm(X17, rr, 0, mask_bits - 1));
                            words.push(enc::asrv(rd, X16, X17));
                            emit_narrow(&mut words, rd, bits);
                        }
                    }
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Rotr => {
                    let at = view.data[inst] as usize;
                    let (rd, sp) = dst_gpr(inst);
                    let rl = load_gpr(&mut words, view.extra[at], X16);
                    let rr = load_gpr(&mut words, view.extra[at + 1], X17);
                    words.push(enc::rorv(rd, rl, rr));
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Udiv => {
                    let at = view.data[inst] as usize;
                    let (rd, sp) = dst_gpr(inst);
                    let ra = load_gpr(&mut words, view.extra[at], X16);
                    let rb = load_gpr(&mut words, view.extra[at + 1], X17);
                    words.push(enc::cbnz(rb, 2));
                    words.push(enc::brk(BRK_DIV_ZERO));
                    words.push(enc::udiv(rd, ra, rb));
                    emit_narrow(&mut words, rd, bits);
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Urem => {
                    let at = view.data[inst] as usize;
                    let (rd, sp) = dst_gpr(inst);
                    let ra = load_gpr(&mut words, view.extra[at], X16);
                    let rb = load_gpr(&mut words, view.extra[at + 1], X17);
                    words.push(enc::cbnz(rb, 2));
                    words.push(enc::brk(BRK_DIV_ZERO));
                    if rd != X16 && rd != X17 {
                        words.push(enc::udiv(rd, ra, rb));
                        words.push(enc::msub(rd, rd, rb, ra));
                    } else if let Some(s) = sp {
                        // Both inputs may occupy X16/X17: preserve `ra` in our own fresh spill slot.
                        if ra == X16 {
                            emit_st_spill_gpr(&mut words, X16, spill_base, s);
                            words.push(enc::udiv(X16, X16, rb));
                            words.push(enc::mul(X16, X16, rb));
                            emit_ld_spill_gpr(&mut words, X17, spill_base, s);
                            words.push(enc::sub(X16, X17, X16));
                        } else {
                            words.push(enc::udiv(X16, ra, rb));
                            words.push(enc::msub(X16, X16, rb, ra));
                        }
                    } else {
                        words.push(enc::udiv(X16, ra, rb));
                    }
                    emit_narrow(&mut words, rd, bits);
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Sdiv => {
                    let at = view.data[inst] as usize;
                    let (rd, sp) = dst_gpr(inst);
                    let ra = load_gpr(&mut words, view.extra[at], X16);
                    let rb = load_gpr(&mut words, view.extra[at + 1], X17);
                    words.push(enc::cbnz(rb, 2));
                    words.push(enc::brk(BRK_DIV_ZERO));
                    let (a_reg, b_reg) = if bits < 64 {
                        words.push(enc::sext(X16, ra, bits));
                        words.push(enc::sext(X17, rb, bits));
                        (X16, X17)
                    } else {
                        (ra, rb)
                    };
                    // Check MIN / -1 overflow trap.
                    words.push(enc::cmn_imm(b_reg, 1));
                    let skip_ov = words.len();
                    words.push(enc::b_cond(Cond::Ne, 0));
                    let min_val = 1u64 << (bits - 1);
                    let min_signed = if bits == 64 { min_val } else { !(min_val - 1) };
                    let tmp = if a_reg == X16 { X17 } else { X16 };
                    emit_const_u64(&mut words, tmp, min_signed);
                    words.push(enc::cmp(a_reg, tmp));
                    words.push(enc::b_cond(Cond::Ne, 2));
                    words.push(enc::brk(BRK_DIV_OVERFLOW));
                    if tmp == b_reg {
                        // On this path `b_reg` was `-1`, so restore it directly with `movn`.
                        words.push(enc::movn(b_reg, 0, 0));
                    }
                    let after_ov = words.len();
                    words[skip_ov] = enc::b_cond(Cond::Ne, (after_ov - skip_ov) as i32);
                    words.push(enc::sdiv(rd, a_reg, b_reg));
                    emit_narrow(&mut words, rd, bits);
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Srem => {
                    let at = view.data[inst] as usize;
                    let (rd, sp) = dst_gpr(inst);
                    let ra = load_gpr(&mut words, view.extra[at], X16);
                    let rb = load_gpr(&mut words, view.extra[at + 1], X17);
                    words.push(enc::cbnz(rb, 2));
                    words.push(enc::brk(BRK_DIV_ZERO));
                    let (a_reg, b_reg) = if bits < 64 {
                        words.push(enc::sext(X16, ra, bits));
                        words.push(enc::sext(X17, rb, bits));
                        (X16, X17)
                    } else {
                        (ra, rb)
                    };
                    if rd != X16 && rd != X17 {
                        words.push(enc::sdiv(rd, a_reg, b_reg));
                        words.push(enc::msub(rd, rd, b_reg, a_reg));
                    } else if let Some(s) = sp {
                        if a_reg == X16 {
                            emit_st_spill_gpr(&mut words, X16, spill_base, s);
                            words.push(enc::sdiv(X16, X16, b_reg));
                            words.push(enc::mul(X16, X16, b_reg));
                            emit_ld_spill_gpr(&mut words, X17, spill_base, s);
                            words.push(enc::sub(X16, X17, X16));
                        } else {
                            words.push(enc::sdiv(X16, a_reg, b_reg));
                            words.push(enc::msub(X16, X16, b_reg, a_reg));
                        }
                    } else {
                        words.push(enc::sdiv(X16, a_reg, b_reg));
                    }
                    emit_narrow(&mut words, rd, bits);
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                // Scalar Floating-Point operations (`f32` / `f64`).
                Op::Fadd => {
                    let double = fp_ty.unwrap();
                    let ft = if double { Ftype::D } else { Ftype::S };
                    let (vd, sp) = dst_fp(inst);
                    match fusion[inst] {
                        Fusion::Fmadd { a, b: mb, acc } => {
                            let va = load_fp(&mut words, a, V29);
                            let vb = load_fp(&mut words, mb, V30);
                            let vc = load_fp(&mut words, acc, V31);
                            words.push(enc::fmadd(ft, vd, va, vb, vc));
                        }
                        _ => {
                            let at = view.data[inst] as usize;
                            let va = load_fp(&mut words, view.extra[at], V30);
                            let vb = load_fp(&mut words, view.extra[at + 1], V31);
                            words.push(enc::fbin(0b0010, double, vd, va, vb));
                        }
                    }
                    if let Some(s) = sp {
                        emit_st_spill_fp(&mut words, vd, spill_base, s);
                    }
                }
                Op::Fsub => {
                    let double = fp_ty.unwrap();
                    let ft = if double { Ftype::D } else { Ftype::S };
                    let (vd, sp) = dst_fp(inst);
                    match fusion[inst] {
                        Fusion::Fmsub { a, b: mb, acc } => {
                            let va = load_fp(&mut words, a, V29);
                            let vb = load_fp(&mut words, mb, V30);
                            let vc = load_fp(&mut words, acc, V31);
                            words.push(enc::fmsub(ft, vd, va, vb, vc));
                        }
                        _ => {
                            let at = view.data[inst] as usize;
                            let va = load_fp(&mut words, view.extra[at], V30);
                            let vb = load_fp(&mut words, view.extra[at + 1], V31);
                            words.push(enc::fbin(0b0011, double, vd, va, vb));
                        }
                    }
                    if let Some(s) = sp {
                        emit_st_spill_fp(&mut words, vd, spill_base, s);
                    }
                }
                Op::Fmul | Op::Fdiv | Op::Fmaximum | Op::Fminimum | Op::FmaxNum | Op::FminNum => {
                    let double = fp_ty.unwrap();
                    let (vd, sp) = dst_fp(inst);
                    let at = view.data[inst] as usize;
                    let va = load_fp(&mut words, view.extra[at], V30);
                    let vb = load_fp(&mut words, view.extra[at + 1], V31);
                    let opcode = match op {
                        Op::Fmul => 0b0000,
                        Op::Fdiv => 0b0001,
                        Op::Fmaximum => 0b0100,
                        Op::Fminimum => 0b0101,
                        Op::FmaxNum => 0b0110,
                        Op::FminNum => 0b0111,
                        _ => unreachable!(),
                    };
                    words.push(enc::fbin(opcode, double, vd, va, vb));
                    if let Some(s) = sp {
                        emit_st_spill_fp(&mut words, vd, spill_base, s);
                    }
                }
                Op::Fneg => {
                    let double = fp_ty.unwrap();
                    let (vd, sp) = dst_fp(inst);
                    let va = load_fp(&mut words, view.data[inst], V30);
                    words.push(enc::fneg(double, vd, va));
                    if let Some(s) = sp {
                        emit_st_spill_fp(&mut words, vd, spill_base, s);
                    }
                }
                Op::Fabs | Op::Sqrt | Op::Nearest | Op::Ceil | Op::Floor | Op::Ftrunc | Op::Fround => {
                    let double = fp_ty.unwrap();
                    let (vd, sp) = dst_fp(inst);
                    let va = load_fp(&mut words, view.data[inst], V30);
                    let opcode = match op {
                        Op::Fabs => 0b000001,
                        Op::Sqrt => 0b000011,
                        Op::Nearest => 0b001000,
                        Op::Ceil => 0b001001,
                        Op::Floor => 0b001010,
                        Op::Ftrunc => 0b001011,
                        Op::Fround => 0b001100,
                        _ => unreachable!(),
                    };
                    words.push(enc::fp1_op(opcode, double, vd, va));
                    if let Some(s) = sp {
                        emit_st_spill_fp(&mut words, vd, spill_base, s);
                    }
                }
                Op::Fma | Op::Fmuladd => {
                    let double = fp_ty.unwrap();
                    let ft = if double { Ftype::D } else { Ftype::S };
                    let (vd, sp) = dst_fp(inst);
                    let at = view.data[inst] as usize;
                    let va = load_fp(&mut words, view.extra[at], V29);
                    let vb = load_fp(&mut words, view.extra[at + 1], V30);
                    let vc = load_fp(&mut words, view.extra[at + 2], V31);
                    words.push(enc::fmadd(ft, vd, va, vb, vc));
                    if let Some(s) = sp {
                        emit_st_spill_fp(&mut words, vd, spill_base, s);
                    }
                }
                Op::Fpext | Op::Fptrunc => {
                    let to_double = fp_ty.unwrap();
                    let (vd, sp) = dst_fp(inst);
                    let va = load_fp(&mut words, view.data[inst], V30);
                    words.push(enc::fcvt(to_double, vd, va));
                    if let Some(s) = sp {
                        emit_st_spill_fp(&mut words, vd, spill_base, s);
                    }
                }
                Op::SitoFp | Op::UitoFp => {
                    let double = fp_ty.unwrap();
                    let (vd, sp) = dst_fp(inst);
                    let src_v = view.data[inst];
                    let src_bits = Ty(view.tys[src_v as usize]).bits(64);
                    let signed = op == Op::SitoFp;
                    let mut ra = load_gpr(&mut words, src_v, X16);
                    if signed && src_bits < 64 {
                        words.push(enc::sext(X16, ra, src_bits));
                        ra = X16;
                    }
                    words.push(enc::cvtf(signed, true, double, vd, ra));
                    if let Some(s) = sp {
                        emit_st_spill_fp(&mut words, vd, spill_base, s);
                    }
                }
                Op::FptoSi | Op::FptoUi => {
                    let src_v = view.data[inst];
                    let double = is_fp_ty(Ty(view.tys[src_v as usize])).unwrap();
                    let signed = op == Op::FptoSi;
                    let (rd, sp) = dst_gpr(inst);
                    let va = load_fp(&mut words, src_v, V30);
                    words.push(enc::fcvtz(signed, bits == 64, double, rd, va));
                    if bits < 32 {
                        let max = (1u64 << (bits - signed as u32)) - 1;
                        if signed {
                            words.push(enc::sext(rd, rd, 32));
                            emit_const_u64(&mut words, X17, max);
                            words.push(enc::cmp(rd, X17));
                            words.push(enc::csel(rd, X17, rd, Cond::Gt));
                            emit_const_u64(&mut words, X17, (-(max as i64) - 1) as u64);
                            words.push(enc::cmp(rd, X17));
                            words.push(enc::csel(rd, X17, rd, Cond::Lt));
                        } else {
                            emit_const_u64(&mut words, X17, max);
                            words.push(enc::cmp(rd, X17));
                            words.push(enc::csel(rd, X17, rd, Cond::Hi));
                        }
                    }
                    emit_narrow(&mut words, rd, bits);
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Bitcast => {
                    let src_v = view.data[inst];
                    let src_fp = is_fp_ty(Ty(view.tys[src_v as usize]));
                    match (src_fp, fp_ty) {
                        (Some(false), None) => {
                            let (rd, sp) = dst_gpr(inst);
                            let va = load_fp(&mut words, src_v, V30);
                            words.push(enc::fmov_from_s(rd, va));
                            if let Some(s) = sp {
                                emit_st_spill_gpr(&mut words, rd, spill_base, s);
                            }
                        }
                        (Some(true), None) => {
                            let (rd, sp) = dst_gpr(inst);
                            let va = load_fp(&mut words, src_v, V30);
                            words.push(enc::fmov_from_d(rd, va));
                            if let Some(s) = sp {
                                emit_st_spill_gpr(&mut words, rd, spill_base, s);
                            }
                        }
                        (None, Some(false)) => {
                            let (vd, sp) = dst_fp(inst);
                            let ra = load_gpr(&mut words, src_v, X16);
                            words.push(enc::fmov_to_s(vd, ra));
                            if let Some(s) = sp {
                                emit_st_spill_fp(&mut words, vd, spill_base, s);
                            }
                        }
                        (None, Some(true)) => {
                            let (vd, sp) = dst_fp(inst);
                            let ra = load_gpr(&mut words, src_v, X16);
                            words.push(enc::fmov_to_d(vd, ra));
                            if let Some(s) = sp {
                                emit_st_spill_fp(&mut words, vd, spill_base, s);
                            }
                        }
                        (Some(d), Some(_)) => {
                            let (vd, sp) = dst_fp(inst);
                            let va = load_fp(&mut words, src_v, V30);
                            if vd != va {
                                words.push(enc::fmov_reg(d, vd, va));
                            }
                            if let Some(s) = sp {
                                emit_st_spill_fp(&mut words, vd, spill_base, s);
                            }
                        }
                        (None, None) => {
                            let (rd, sp) = dst_gpr(inst);
                            let ra = load_gpr(&mut words, src_v, X16);
                            if rd != ra {
                                words.push(enc::mov(rd, ra));
                            }
                            if let Some(s) = sp {
                                emit_st_spill_gpr(&mut words, rd, spill_base, s);
                            }
                        }
                    }
                }
                Op::Icmp => {
                    let at = view.data[inst] as usize;
                    let cond = IntCond::from_word(view.extra[at]).ok_or(TierError::Refused {
                        tier: TIER,
                        inst: Some(inst as u32),
                        what: "invalid icmp cond",
                    })?;
                    let a = view.extra[at + 1];
                    let bv = view.extra[at + 2];
                    let (rd, sp) = dst_gpr(inst);
                    emit_cmp(&mut words, cond, a, bv, Ty(view.tys[a as usize]));
                    words.push(enc::cset(rd, cond_of(cond)));
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Fcmp => {
                    let at = view.data[inst] as usize;
                    let cond = FloatCond::from_word(view.extra[at]).ok_or(TierError::Refused {
                        tier: TIER,
                        inst: Some(inst as u32),
                        what: "invalid fcmp cond",
                    })?;
                    let a = view.extra[at + 1];
                    let bv = view.extra[at + 2];
                    let double = is_fp_ty(Ty(view.tys[a as usize])).unwrap();
                    let (rd, sp) = dst_gpr(inst);
                    match cond {
                        FloatCond::False => words.push(enc::movz(rd, 0, 0)),
                        FloatCond::True => words.push(enc::movz(rd, 1, 0)),
                        FloatCond::One => {
                            let va = load_fp(&mut words, a, V30);
                            let vb = load_fp(&mut words, bv, V31);
                            words.push(enc::fcmp(double, va, vb));
                            words.push(enc::cset(rd, Cond::Mi));
                            words.push(enc::cset(X17, Cond::Gt));
                            words.push(enc::orr(rd, rd, X17));
                        }
                        FloatCond::Ueq => {
                            let va = load_fp(&mut words, a, V30);
                            let vb = load_fp(&mut words, bv, V31);
                            words.push(enc::fcmp(double, va, vb));
                            words.push(enc::cset(rd, Cond::Eq));
                            words.push(enc::cset(X17, Cond::Vs));
                            words.push(enc::orr(rd, rd, X17));
                        }
                        other => {
                            let va = load_fp(&mut words, a, V30);
                            let vb = load_fp(&mut words, bv, V31);
                            words.push(enc::fcmp(double, va, vb));
                            words.push(enc::cset(rd, fcond_of(other).unwrap()));
                        }
                    }
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Select => {
                    let at = view.data[inst] as usize;
                    let t = view.extra[at + 1];
                    let f = view.extra[at + 2];
                    let sel_cond = match fusion[inst] {
                        Fusion::CmpSelect { cond, a, b: bv, op_ty } => {
                            emit_cmp(&mut words, cond, a, bv, op_ty);
                            cond_of(cond)
                        }
                        Fusion::FcmpSelect { cond, a, b: bv, double } => {
                            let va = load_fp(&mut words, a, V30);
                            let vb = load_fp(&mut words, bv, V31);
                            words.push(enc::fcmp(double, va, vb));
                            cond
                        }
                        _ => {
                            let rc = load_gpr(&mut words, view.extra[at], X16);
                            words.push(enc::cmp_imm(rc, 0));
                            Cond::Ne
                        }
                    };
                    // `ldr` does not modify NZCV flags, so reloading `t`/`f` preserves `sel_cond`!
                    if let Some(double) = fp_ty {
                        let (vd, sp) = dst_fp(inst);
                        let vt = load_fp(&mut words, t, V30);
                        let vf = load_fp(&mut words, f, V31);
                        words.push(enc::fcsel(double, vd, vt, vf, sel_cond));
                        if let Some(s) = sp {
                            emit_st_spill_fp(&mut words, vd, spill_base, s);
                        }
                    } else {
                        let (rd, sp) = dst_gpr(inst);
                        let rt = load_gpr(&mut words, t, X16);
                        let rf = load_gpr(&mut words, f, X17);
                        words.push(enc::csel(rd, rt, rf, sel_cond));
                        if let Some(s) = sp {
                            emit_st_spill_gpr(&mut words, rd, spill_base, s);
                        }
                    }
                }
                Op::Zext => {
                    let (rd, sp) = dst_gpr(inst);
                    let src = load_gpr(&mut words, view.data[inst], X16);
                    if rd != src {
                        words.push(enc::mov(rd, src));
                    }
                    emit_narrow(&mut words, rd, bits);
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Sext => {
                    let src_v = view.data[inst];
                    let src_bits = Ty(view.tys[src_v as usize]).bits(64);
                    let (rd, sp) = dst_gpr(inst);
                    let src = load_gpr(&mut words, src_v, X16);
                    if src_bits < 64 {
                        words.push(enc::sext(rd, src, src_bits));
                    } else if rd != src {
                        words.push(enc::mov(rd, src));
                    }
                    emit_narrow(&mut words, rd, bits);
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Trunc => {
                    let (rd, sp) = dst_gpr(inst);
                    let src = load_gpr(&mut words, view.data[inst], X16);
                    if bits < 64 {
                        words.push(enc::zext(rd, src, bits));
                    } else if rd != src {
                        words.push(enc::mov(rd, src));
                    }
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Call | Op::CallIndirect => {
                    let at = view.data[inst] as usize;
                    let sig_id = SigId(view.extra[at + 1]);
                    let sig = view.sig(sig_id);
                    let argc = view.extra[at + 2] as usize;

                    let direct_target = if op == Op::Call {
                        let rec = view.reference(RefId(view.extra[at]));
                        Some(resolve_outside(names, Outside::Func(rec.place), Space::Func, true, &mut out, inst)?)
                    } else {
                        None
                    };

                    // Marshal call arguments into `X0..X7` and `V0..V7` (and `X17` for `CallIndirect` callee).
                    gpr_moves.clear();
                    fp_moves.clear();
                    const_edge_writes.clear();
                    if op == Op::CallIndirect {
                        let callee_v = view.extra[at];
                        match reg_alloc.loc_of[callee_v as usize] {
                            AllocLoc::Gpr(s) => gpr_moves.push((GprCopyLoc::Reg(X17), GprCopyLoc::Reg(u32::from(s)))),
                            AllocLoc::GprSpill(s) => gpr_moves.push((GprCopyLoc::Reg(X17), GprCopyLoc::Spill(s))),
                            other => panic!("Sota internal error: indirect callee got {other:?}"),
                        }
                    }
                    let mut gpr_k = 0usize;
                    let mut fp_k = 0usize;
                    let mut sext_args: [(Reg, u32); 8] = [(0, 0); 8];
                    let mut sext_n = 0usize;
                    for k in 0..argc {
                        let arg_v = view.extra[at + 3 + k];
                        let param = sig.params[k];
                        let arg_ty = Ty(view.tys[arg_v as usize]);
                        if is_fp_ty(arg_ty).is_some() {
                            let dst = FpCopyLoc::Reg(FP_ARG_REGS[fp_k]);
                            fp_k += 1;
                            match reg_alloc.loc_of[arg_v as usize] {
                                AllocLoc::Fp(s) => fp_moves.push((dst, FpCopyLoc::Reg(u32::from(s)))),
                                AllocLoc::FpSpill(s) => fp_moves.push((dst, FpCopyLoc::Spill(s))),
                                other => panic!("Sota internal error: FP call arg got {other:?}"),
                            }
                        } else {
                            let dst_reg = ARG_REGS[gpr_k];
                            let dst = GprCopyLoc::Reg(dst_reg);
                            gpr_k += 1;
                            match reg_alloc.loc_of[arg_v as usize] {
                                AllocLoc::Gpr(s) => gpr_moves.push((dst, GprCopyLoc::Reg(u32::from(s)))),
                                AllocLoc::GprSpill(s) => gpr_moves.push((dst, GprCopyLoc::Spill(s))),
                                AllocLoc::None => {
                                    let cval = const_u64[arg_v as usize].expect("unallocated non-const call arg");
                                    const_edge_writes.push((dst, cval));
                                }
                                other => panic!("Sota internal error: GPR call arg got {other:?}"),
                            }
                            if param.ext == Ext::Sext as u8 {
                                let w = arg_ty.bits(64);
                                if w < 64 {
                                    sext_args[sext_n] = (dst_reg, w);
                                    sext_n += 1;
                                }
                            }
                        }
                    }
                    emit_parallel_gpr_moves(&mut words, &mut gpr_moves, spill_base);
                    emit_parallel_fp_moves(&mut words, &mut fp_moves, spill_base);
                    for &(dst, cval) in const_edge_writes.iter() {
                        if let GprCopyLoc::Reg(r) = dst {
                            emit_const_u64(&mut words, r, cval);
                        }
                    }
                    for &(reg, w) in &sext_args[..sext_n] {
                        words.push(enc::sext(reg, reg, w));
                    }

                    if let Some(target) = direct_target {
                        let adrp_at = (words.len() * 4) as u32;
                        words.push(enc::adrp(X16, 0));
                        words.push(enc::ldr(Size::X, X16, X16, 0));
                        out.relocs.push(Reloc { at: adrp_at, in_relro: false, kind: RelocKind::GotPage21, target, addend: 0 });
                        out.relocs.push(Reloc { at: adrp_at + 4, in_relro: false, kind: RelocKind::GotPageOff12, target, addend: 0 });
                        let blr_at = (words.len() * 4) as u32;
                        let row = blaze_code::unwind::Row {
                            start: blr_at,
                            len: 4,
                            landing: 0,
                            action: blaze_code::unwind::Action::Continue,
                        };
                        out.unwind.push(row);
                        out.pads.push(row.pad());
                        words.push(enc::blr(X16));
                    } else {
                        let blr_at = (words.len() * 4) as u32;
                        let row = blaze_code::unwind::Row {
                            start: blr_at,
                            len: 4,
                            landing: 0,
                            action: blaze_code::unwind::Action::Continue,
                        };
                        out.unwind.push(row);
                        out.pads.push(row.pad());
                        words.push(enc::blr(X17));
                    }

                    if loc != AllocLoc::None && ty != Ty::NONE {
                        if let Some(double) = fp_ty {
                            match loc {
                                AllocLoc::Fp(dst) => {
                                    if u32::from(dst) != 0 {
                                        words.push(enc::fmov_reg(double, u32::from(dst), 0));
                                    }
                                }
                                AllocLoc::FpSpill(s) => emit_st_spill_fp(&mut words, 0, spill_base, s),
                                _ => {}
                            }
                        } else {
                            match loc {
                                AllocLoc::Gpr(dst) => {
                                    let rd = u32::from(dst);
                                    if rd != X0 {
                                        words.push(enc::mov(rd, X0));
                                    }
                                    emit_narrow(&mut words, rd, bits);
                                }
                                AllocLoc::GprSpill(s) => {
                                    emit_narrow(&mut words, X0, bits);
                                    emit_st_spill_gpr(&mut words, X0, spill_base, s);
                                }
                                _ => {}
                            }
                        }
                    }
                }
                Op::Jump => {
                    let at = view.data[inst] as usize;
                    let target_block = view.extra[at];
                    let argc = view.extra[at + 1] as usize;
                    emit_edge_moves(&mut words, &mut gpr_moves, &mut fp_moves, &mut const_edge_writes, target_block, at + 2, argc);
                    if target_block as usize != b + 1 {
                        let widx = words.len();
                        words.push(enc::b(0));
                        patches.push(BranchPatch { word_idx: widx, kind: PatchKind::B, target_block });
                    }
                }
                Op::Br => {
                    let at = view.data[inst] as usize;
                    let cond_v = view.extra[at];
                    let then_block = view.extra[at + 1];
                    let then_argc = view.extra[at + 2] as usize;
                    let else_at = at + 3 + then_argc;
                    let else_block = view.extra[else_at];
                    let else_argc = view.extra[else_at + 1] as usize;

                    if then_argc == 0 && else_argc == 0 {
                        match fusion[inst] {
                            Fusion::CmpBr { cond, a, b: bv, op_ty } => {
                                let is_zero_cmp = const_u64[bv as usize] == Some(0)
                                    && matches!(cond, IntCond::Eq | IntCond::Ne);
                                if is_zero_cmp {
                                    let ra = load_gpr(&mut words, a, X16);
                                    if else_block as usize == b + 1 {
                                        let kind = if cond == IntCond::Eq { PatchKind::Cbz(ra) } else { PatchKind::Cbnz(ra) };
                                        let widx = words.len();
                                        words.push(enc::cbz(ra, 0));
                                        patches.push(BranchPatch { word_idx: widx, kind, target_block: then_block });
                                    } else if then_block as usize == b + 1 {
                                        let kind = if cond == IntCond::Eq { PatchKind::Cbnz(ra) } else { PatchKind::Cbz(ra) };
                                        let widx = words.len();
                                        words.push(enc::cbnz(ra, 0));
                                        patches.push(BranchPatch { word_idx: widx, kind, target_block: else_block });
                                    } else {
                                        let kind = if cond == IntCond::Eq { PatchKind::Cbz(ra) } else { PatchKind::Cbnz(ra) };
                                        let w1 = words.len();
                                        words.push(enc::cbz(ra, 0));
                                        patches.push(BranchPatch { word_idx: w1, kind, target_block: then_block });
                                        let w2 = words.len();
                                        words.push(enc::b(0));
                                        patches.push(BranchPatch { word_idx: w2, kind: PatchKind::B, target_block: else_block });
                                    }
                                } else {
                                    emit_cmp(&mut words, cond, a, bv, op_ty);
                                    emit_cond_branch_pair(&mut words, &mut patches, cond_of(cond), then_block, else_block, b);
                                }
                            }
                            Fusion::FcmpBr { cond, a, b: bv, double } => {
                                let va = load_fp(&mut words, a, V30);
                                let vb = load_fp(&mut words, bv, V31);
                                words.push(enc::fcmp(double, va, vb));
                                emit_cond_branch_pair(&mut words, &mut patches, cond, then_block, else_block, b);
                            }
                            _ => {
                                let rc = load_gpr(&mut words, cond_v, X16);
                                if else_block as usize == b + 1 {
                                    let widx = words.len();
                                    words.push(enc::cbnz(rc, 0));
                                    patches.push(BranchPatch { word_idx: widx, kind: PatchKind::Cbnz(rc), target_block: then_block });
                                } else if then_block as usize == b + 1 {
                                    let widx = words.len();
                                    words.push(enc::cbz(rc, 0));
                                    patches.push(BranchPatch { word_idx: widx, kind: PatchKind::Cbz(rc), target_block: else_block });
                                } else {
                                    let w1 = words.len();
                                    words.push(enc::cbnz(rc, 0));
                                    patches.push(BranchPatch { word_idx: w1, kind: PatchKind::Cbnz(rc), target_block: then_block });
                                    let w2 = words.len();
                                    words.push(enc::b(0));
                                    patches.push(BranchPatch { word_idx: w2, kind: PatchKind::B, target_block: else_block });
                                }
                            }
                        }
                    } else {
                        let skip_then = match fusion[inst] {
                            Fusion::CmpBr { cond, a, b: bv, op_ty } => {
                                emit_cmp(&mut words, cond, a, bv, op_ty);
                                let inv = invert_cond(cond_of(cond));
                                let idx = words.len();
                                words.push(enc::b_cond(inv, 0));
                                (idx, PatchKind::BCond(inv))
                            }
                            Fusion::FcmpBr { cond, a, b: bv, double } => {
                                let va = load_fp(&mut words, a, V30);
                                let vb = load_fp(&mut words, bv, V31);
                                words.push(enc::fcmp(double, va, vb));
                                let inv = invert_cond(cond);
                                let idx = words.len();
                                words.push(enc::b_cond(inv, 0));
                                (idx, PatchKind::BCond(inv))
                            }
                            _ => {
                                let rc = load_gpr(&mut words, cond_v, X16);
                                let idx = words.len();
                                words.push(enc::cbz(rc, 0));
                                (idx, PatchKind::Cbz(rc))
                            }
                        };
                        emit_edge_moves(&mut words, &mut gpr_moves, &mut fp_moves, &mut const_edge_writes, then_block, at + 3, then_argc);
                        let w_then = words.len();
                        words.push(enc::b(0));
                        patches.push(BranchPatch { word_idx: w_then, kind: PatchKind::B, target_block: then_block });

                        let else_start = words.len();
                        let delta = (else_start - skip_then.0) as i32;
                        words[skip_then.0] = match skip_then.1 {
                            PatchKind::BCond(inv) => enc::b_cond(inv, delta),
                            PatchKind::Cbz(rc) => enc::cbz(rc, delta),
                            _ => unreachable!(),
                        };
                        emit_edge_moves(&mut words, &mut gpr_moves, &mut fp_moves, &mut const_edge_writes, else_block, else_at + 2, else_argc);
                        if else_block as usize != b + 1 {
                            let w_else = words.len();
                            words.push(enc::b(0));
                            patches.push(BranchPatch { word_idx: w_else, kind: PatchKind::B, target_block: else_block });
                        }
                    }
                }
                Op::Switch => {
                    let at = view.data[inst] as usize;
                    let scrutinee = view.extra[at];
                    let default_block = view.extra[at + 1];
                    let default_argc = view.extra[at + 2] as usize;
                    let cases_at = at + 3 + default_argc;
                    let num_cases = view.extra[cases_at] as usize;
                    let width_words = Ty(view.tys[scrutinee as usize]).const_words() as usize;

                    let sr = load_gpr(&mut words, scrutinee, X16);
                    let mut pos = cases_at + 1;
                    for _ in 0..num_cases {
                        let mut case_val = 0u64;
                        for w in (0..width_words).rev() {
                            case_val = (case_val << 32) | u64::from(view.extra[pos + w]);
                        }
                        pos += width_words;
                        let case_block = view.extra[pos];
                        let case_argc = view.extra[pos + 1] as usize;
                        let case_args_start = pos + 2;
                        pos += 2 + case_argc;

                        if case_val <= 4095 {
                            words.push(enc::cmp_imm(sr, case_val as u32));
                        } else {
                            emit_const_u64(&mut words, X17, case_val);
                            words.push(enc::cmp(sr, X17));
                        }
                        if case_argc == 0 {
                            let widx = words.len();
                            words.push(enc::b_cond(Cond::Eq, 0));
                            patches.push(BranchPatch { word_idx: widx, kind: PatchKind::BCond(Cond::Eq), target_block: case_block });
                        } else {
                            let skip_idx = words.len();
                            words.push(enc::b_cond(Cond::Ne, 0));
                            emit_edge_moves(&mut words, &mut gpr_moves, &mut fp_moves, &mut const_edge_writes, case_block, case_args_start, case_argc);
                            let w_jump = words.len();
                            words.push(enc::b(0));
                            patches.push(BranchPatch { word_idx: w_jump, kind: PatchKind::B, target_block: case_block });
                            let after = words.len();
                            words[skip_idx] = enc::b_cond(Cond::Ne, (after - skip_idx) as i32);
                        }
                    }
                    emit_edge_moves(&mut words, &mut gpr_moves, &mut fp_moves, &mut const_edge_writes, default_block, at + 3, default_argc);
                    if default_block as usize != b + 1 {
                        let widx = words.len();
                        words.push(enc::b(0));
                        patches.push(BranchPatch { word_idx: widx, kind: PatchKind::B, target_block: default_block });
                    }
                }
                Op::Return => {
                    let at = view.data[inst] as usize;
                    let retc = view.extra[at] as usize;
                    if retc == 1 {
                        let rv = view.extra[at + 1];
                        let rv_ty = Ty(view.tys[rv as usize]);
                        if let Some(double) = is_fp_ty(rv_ty) {
                            match reg_alloc.loc_of[rv as usize] {
                                AllocLoc::Fp(s) => {
                                    if u32::from(s) != 0 {
                                        words.push(enc::fmov_reg(double, 0, u32::from(s)));
                                    }
                                }
                                AllocLoc::FpSpill(s) => emit_ld_spill_fp(&mut words, 0, spill_base, s),
                                other => panic!("Sota internal error: FP return got {other:?}"),
                            }
                        } else {
                            match reg_alloc.loc_of[rv as usize] {
                                AllocLoc::Gpr(s) => {
                                    if u32::from(s) != X0 {
                                        words.push(enc::mov(X0, u32::from(s)));
                                    }
                                }
                                AllocLoc::GprSpill(s) => emit_ld_spill_gpr(&mut words, X0, spill_base, s),
                                AllocLoc::None => {
                                    let cval = const_u64[rv as usize].expect("unallocated non-const return");
                                    emit_const_u64(&mut words, X0, cval);
                                }
                                other => panic!("Sota internal error: GPR return got {other:?}"),
                            }
                            if func_sig.rets[0].ext == Ext::Sext as u8 {
                                let w = rv_ty.bits(64);
                                if w < 64 {
                                    words.push(enc::sext(X0, X0, w));
                                }
                            }
                        }
                    }
                    if needs_frame {
                        if frame_size > 0 || most_align > 16 {
                            if cs_bytes == 0 {
                                words.push(enc::add_imm(SP, FP, 0, false));
                            } else {
                                words.push(enc::sub_imm(SP, FP, cs_bytes, false));
                            }
                        }
                        for k in (0..4u32).rev() {
                            if d_pairs & (1 << k) != 0 {
                                let lo = 8 + 2 * k;
                                let hi = lo + 1;
                                words.push(enc::ldp_post_d(hi, lo, SP, 16));
                            }
                        }
                        for k in (0..5u32).rev() {
                            if x_pairs & (1 << k) != 0 {
                                let lo = 19 + 2 * k;
                                let hi = lo + 1;
                                words.push(enc::ldp_post(hi, lo, SP, 16));
                            }
                        }
                        words.push(enc::ldp_post(FP, LR, SP, 16));
                    }
                    words.push(enc::ret());
                }
                Op::Trap => {
                    words.push(enc::brk(view.data[inst]));
                }
                Op::Unreachable => {
                    words.push(enc::brk(BRK_UNREACHABLE));
                }
                _ => {
                    return Err(TierError::Refused { tier: TIER, inst: Some(inst as u32), what: op.name() });
                }
            }
        }
    }

    // 5. Patch all intra-function branch displacements.
    for p in &patches {
        let target_w = block_word_off[p.target_block as usize];
        let delta = target_w as i32 - p.word_idx as i32;
        words[p.word_idx] = match p.kind {
            PatchKind::B => enc::b(delta),
            PatchKind::BCond(c) => enc::b_cond(c, delta),
            PatchKind::Cbz(r) => enc::cbz(r, delta),
            PatchKind::Cbnz(r) => enc::cbnz(r, delta),
        };
    }

    // 6. Encode AArch64 words little-endian into `Emitted.code`.
    out.code.reserve(words.len() * 4);
    for &w in &words {
        out.code.extend_from_slice(&w.to_le_bytes());
    }
    Ok(out)
}

#[inline]
fn emit_cond_branch_pair<'a>(
    words: &mut Vec<u32, Alloc<'a>>,
    patches: &mut Vec<BranchPatch, Alloc<'a>>,
    cond: Cond,
    then_block: u32,
    else_block: u32,
    b: usize,
) {
    if else_block as usize == b + 1 {
        let widx = words.len();
        words.push(enc::b_cond(cond, 0));
        patches.push(BranchPatch { word_idx: widx, kind: PatchKind::BCond(cond), target_block: then_block });
    } else if then_block as usize == b + 1 {
        let inv = invert_cond(cond);
        let widx = words.len();
        words.push(enc::b_cond(inv, 0));
        patches.push(BranchPatch { word_idx: widx, kind: PatchKind::BCond(inv), target_block: else_block });
    } else {
        let w1 = words.len();
        words.push(enc::b_cond(cond, 0));
        patches.push(BranchPatch { word_idx: w1, kind: PatchKind::BCond(cond), target_block: then_block });
        let w2 = words.len();
        words.push(enc::b(0));
        patches.push(BranchPatch { word_idx: w2, kind: PatchKind::B, target_block: else_block });
    }
}
