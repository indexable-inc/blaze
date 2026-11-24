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

use blaze_code::record::{
    Held, OWN_RELRO, PositionField, PositionPart, PositionRow, Space, Symbol, Target, TrapKind, TrapRow, Word,
};
use blaze_lir::{
    AtomicRmw, Block, BlockKind, Flags, FloatCond, IntCond, Op, Ordering, Ty, View,
    func::{Conv, DataId, Ext, IntrinsicId, ParamKind, RefId, RefKind, SigId},
};
use blaze_own::{Alloc, Vec};
use blaze_tier::{CallClobbers, Emitted, Named, Names, Outside, Reloc, RelocKind, TierError, TierName};

use super::CallMode;
use super::regalloc::{self, ARG_REGS, AllocLoc, FP_ARG_REGS, is_fp_ty, is_gpr_pair_ty, is_vec128_ty, is_vec_mask_ty};
use super::release_opts::{
    MagicUdiv64, compute_block_order, compute_magic_udiv64, dense_switch_plan, detect_tail_calls,
};
use crate::enc::{
    self, Barrier, Cond, FP, Ftype, LR, Lse, Reg, SP, Size, X0, X1, X2, X8, X9, X10, X11, X12, X13, X14, X16, ZR,
};
use crate::{enc_probe, enc_tls};
use crate::pass::{
    BRK_DIV_OVERFLOW, BRK_DIV_ZERO, BRK_UNREACHABLE,
    probe::PROBE_STEP,
    stack::{MOST_AREA, Nsaa, Stacked},
    tls_index::{DESC_ROOT_WORDS, TABLE_LEN_WORDS, TABLE_SLOTS},
};

const X3: Reg = 3;
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
const MOST_DATA_ALIGN: u8 = blaze_code::record::MOST_PART_ALIGN;
/// Maximum byte length for `Sota`'s inline NEON 64B/iter `Memcpy`, 32B/instr `Memset`, and 16B/iter `Memcmp`.
const SOTA_BULK_INLINE_MOST: u64 = 256;

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
fn word_delta(target: usize, from: usize) -> i32 {
    if target >= from {
        i32_of(target - from)
    } else {
        i32_of(from - target).wrapping_neg()
    }
}

#[inline]
const fn mem_shift(size: Size) -> u32 {
    match size {
        Size::B => 0,
        Size::H => 1,
        Size::W => 2,
        Size::X => 3,
    }
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
fn at_opt<T: Copy>(s: &[Option<T>], i: usize) -> Option<T> {
    s.get(i).copied().flatten()
}

#[inline]
fn at_loc(s: &[AllocLoc], i: usize) -> AllocLoc {
    s.get(i).copied().unwrap_or(AllocLoc::None)
}

#[inline]
fn at_fusion(s: &[Fusion], i: usize) -> Fusion {
    s.get(i).copied().unwrap_or(Fusion::None)
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
    /// `xor(a, shl(b, k))` -> `eor rd, a, b, lsl #k`.
    EorLsl { a: u32, b: u32, shift: u32, imm_inst: u32 },
    /// `xor(a, lshr(b, k))` -> `eor rd, a, b, lsr #k`.
    EorLsr { a: u32, b: u32, shift: u32, imm_inst: u32 },
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
    /// `load`/`store` directly from `[base, idx, lsl #size]` (scaled register index).
    MemScaledIdx { base: u32, idx: u32, shift_imm: u32 },
    /// `load`/`store` directly from `[base, idx]` (unscaled register index).
    MemRegIdx { base: u32, idx: u32 },
    /// `and(a, not(b))` -> `bic rd, a, b`.
    Bic { a: u32, b: u32 },
    /// `or(a, not(b))` -> `orn rd, a, b`.
    Orn { a: u32, b: u32 },
    /// `xor(a, not(b))` -> `eon rd, a, b`.
    Eon { a: u32, b: u32 },
    /// `br(icmp eq/ne (and(val, 1 << bit)), 0)` -> single `tbz`/`tbnz`.
    TbzBr { val: u32, bit: u32, nonzero_is_then: bool, mask_imm: u32 },
    /// `br(icmp eq/ne (and(a, mask)), 0)` -> `tst` + `b.eq`/`b.ne`.
    TstBr { a: u32, rhs: u32, imm_nrs: Option<u32>, nonzero_is_then: bool },
    /// `select(icmp, x, neg(x))` -> `cmp` + `csneg`.
    CsnegSelect { cond: IntCond, a: u32, b: u32, op_ty: Ty, x: u32, neg_when_false: bool },
    /// `select(icmp, x, add(x, 1))` -> `cmp` + `csinc`.
    CsincSelect { cond: IntCond, a: u32, b: u32, op_ty: Ty, x: u32, inc_when_false: bool },
    /// `select(icmp, x, not(x))` -> `cmp` + `csinv`.
    CsinvSelect { cond: IntCond, a: u32, b: u32, op_ty: Ty, x: u32, inv_when_false: bool },
    /// ARMv8.2-SHA3 `xor(xor(a, b), c)` on 128-bit vectors -> 1-instruction `eor3 vd.16b, va, vb, vc`!
    Eor3 { a: u32, b: u32, c: u32 },
    /// ARMv8.2-SHA3 `xor(a, and(b, not(c)))` on 128-bit vectors -> 1-instruction `bcax vd.16b, va, vb, vc`!
    Bcax { a: u32, b: u32, c: u32 },
    /// ARMv8.2-SHA3 `xor(a, rotl(b, 1))` on `<2 x i64>` -> 1-instruction `rax1 vd.2d, va, vb`!
    Rax1 { a: u32, b: u32 },
    /// Multi-limb `U256`/`U128` carry-chain add `add(add(a, b), zext(carry))` -> 1-instruction `adcs rd, ra, rb`!
    AdcsChain { a: u32, b: u32, carry_src: u32 },
    /// Multi-limb `U256` overflowing carry-chain limb (`uadd_overflow(a, b) + carry_in -> (sum, carry_out)`)
    /// fused directly into 1-instruction `adcs rd, ra, rb`!
    AdcsOverflowChain { a: u32, b: u32, carry_src: u32, carry_out_or: Option<u32> },
    /// Zero-branch `Conv::BlazeResult` call immediately followed by `br(project(call, 1), err, ok)`
    /// -> single-instruction `b.cs err` directly on the callee's Carry flag (0 tag reg, 0 `cset`)!
    BlazeResultCallBr,
    /// `<2 x i64>` widening pairwise accumulate `add(add(acc, and(bitcast(e: <4 x i32>), 0xffff_ffff)), lshr(bitcast(e), 32))`
    /// -> 1-instruction `uadalp.2d vacc, vsrc.4s`!
    VecUadalp { acc: u32, src_4s: u32 },
}

/// Branch patch site resolved once every block's word offset is known.
#[derive(Clone, Copy, Debug)]
enum PatchKind {
    B,
    BCond(Cond),
    Cbz(Reg),
    Cbnz(Reg),
    Tbz(Reg, u32),
    Tbnz(Reg, u32),
    /// 32-bit signed byte offset entry inside an inline PC-relative jump table (`table_start_word`).
    JumpTableEntry { table_start_word: usize },
}

#[derive(Clone, Copy, Debug)]
struct BranchPatch {
    word_idx: usize,
    kind: PatchKind,
    target_block: u32,
}

/// Source or destination location for a parallel move (register, 8-byte spill slot, or 16-byte SIMD spill),
/// tagged by register bank (`FP = false` for GPR, `FP = true` for FP/SIMD).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CopyLoc<const FP: bool> {
    Reg(Reg),
    Spill(u32),
    VecSpill(u32),
}

type GprCopyLoc = CopyLoc<false>;
type FpCopyLoc = CopyLoc<true>;

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
    let bytes = 1i64 << mem_shift(size);
    (off >= 0 && off % bytes == 0 && off / bytes < 4096) || (-256..=255).contains(&off)
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
            let v = if bits <= 32 {
                u64::from(w)
            } else {
                i64::from(w.cast_signed()).cast_unsigned()
            };
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

#[inline]
fn sign_extend_u64(val: u64, bits: u32) -> i64 {
    if bits >= 64 {
        val.cast_signed()
    } else {
        let shift = 64 - bits;
        (val << shift).cast_signed() >> shift
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
        let chunk = trunc_u32((inv >> (hw * 16)) & 0xffff);
        if inv == (u64::from(chunk) << (hw * 16)) {
            words.push(enc::movn(rd, chunk, hw));
            return;
        }
    }
    let mut first = true;
    for hw in 0..4u32 {
        let chunk = trunc_u32((val >> (hw * 16)) & 0xffff);
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
    let bytes = 1i64 << mem_shift(size);
    let raw = if off >= 0 && off % bytes == 0 && off / bytes < 4096 {
        let imm12 = u32_of(off / bytes);
        if load {
            enc::ldr(size, rt, rn, imm12)
        } else {
            enc::str(size, rt, rn, imm12)
        }
    } else {
        debug_assert!((-256..=255).contains(&off));
        let off_i32 = i32_of(off);
        if load {
            enc::ldur(size, rt, rn, off_i32)
        } else {
            enc::stur(size, rt, rn, off_i32)
        }
    };
    words.push(if fp { enc::fp_access(raw) } else { raw });
}

/// Emits a load/store at `[rn, #off]` for any non-negative `off < 16 MiB` (even unaligned offsets),
/// using `X17` as scratch base if `off` does not fit an unsigned scaled immediate or `ldur`/`stur`.
fn emit_mem_off(words: &mut Vec<u32, Alloc<'_>>, load: bool, fp: bool, size: Size, rt: Reg, rn: Reg, off: u32) {
    debug_assert!(fp || rt != X17);
    let bytes = 1u32 << mem_shift(size);
    if (off % bytes == 0 && off / bytes < 4096) || off <= 255 {
        emit_mem_imm(words, load, fp, size, rt, rn, i64::from(off));
    } else {
        let (hi, lo) = (off >> 12, off & 0xfff);
        if hi != 0 {
            words.push(enc::add_imm(X17, rn, hi, true));
            if lo != 0 {
                words.push(enc::add_imm(X17, X17, lo, false));
            }
        } else {
            words.push(enc::add_imm(X17, rn, lo, false));
        }
        let raw = if load { enc::ldr(size, rt, X17, 0) } else { enc::str(size, rt, X17, 0) };
        words.push(if fp { enc::fp_access(raw) } else { raw });
    }
}

/// Copies `size` bytes from `src_reg` to `[sp + dst_sp_off]` without clobbering any caller-saved
/// or callee-saved register (`X0..X15`, `X19..X28`, `V0..V29`) or leaving `sp` shifted.
fn emit_byval_copy(words: &mut Vec<u32, Alloc<'_>>, dst_sp_off: u32, src_reg: Reg, size: u32) {
    if size == 0 {
        return;
    }
    if src_reg != X17 {
        words.push(enc::mov(X17, src_reg));
    }
    emit_sp_addr(words, X16, dst_sp_off);
    let mut rem = size;
    let mut off = 0u32;
    if u64::from(rem) > SOTA_BULK_INLINE_MOST {
        let chunks = rem / 16;
        words.push(enc::stp_pre(14, 15, SP, -16));
        emit_const_u64(words, 14, u64::from(chunks));
        let loop_idx = words.len();
        words.push(enc::ldr_q(V30, X17, 0));
        words.push(enc::str_q(V30, X16, 0));
        words.push(enc::add_imm(X17, X17, 16, false));
        words.push(enc::add_imm(X16, X16, 16, false));
        words.push(enc::sub_imm(14, 14, 1, false));
        let back = word_delta(loop_idx, words.len());
        words.push(enc::cbnz(14, back));
        words.push(enc::ldp_post(14, 15, SP, 16));
        rem %= 16;
    } else {
        while rem >= 16 {
            words.push(enc::ldr_q(V30, X17, off / 16));
            words.push(enc::str_q(V30, X16, off / 16));
            off += 16;
            rem -= 16;
        }
    }
    for (bytes, sz) in [(8u32, Size::X), (4, Size::W), (2, Size::H), (1, Size::B)] {
        if rem >= bytes {
            emit_mem_imm(words, true, true, sz, V30, X17, i64::from(off));
            emit_mem_imm(words, false, true, sz, V30, X16, i64::from(off));
            off += bytes;
            rem -= bytes;
        }
    }
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

/// Emits a 128-bit NEON vector load (`is_load = true`) or store (`is_load = false`) at `[rn, #off]`.
fn emit_mem_q(words: &mut Vec<u32, Alloc<'_>>, is_load: bool, vt: Reg, rn: Reg, off: i64) {
    if off >= 0 && off % 16 == 0 && off / 16 < 4096 {
        let imm12 = u32_of(off / 16);
        words.push(if is_load { enc::ldr_q(vt, rn, imm12) } else { enc::str_q(vt, rn, imm12) });
    } else if (-256..=255).contains(&off) {
        let off_i32 = i32_of(off);
        words.push(if is_load { enc::ldur_q(vt, rn, off_i32) } else { enc::stur_q(vt, rn, off_i32) });
    } else {
        emit_const_u64(words, X17, off.cast_unsigned());
        words.push(enc::add(X17, rn, X17));
        words.push(if is_load { enc::ldr_q(vt, X17, 0) } else { enc::str_q(vt, X17, 0) });
    }
}

/// Loads a 16-byte SIMD vector spill slot pair at `sp + spill_base + slot * 8`.
#[inline]
fn emit_ld_spill_vec(words: &mut Vec<u32, Alloc<'_>>, vd: Reg, spill_base: u32, slot: u32) {
    let off = i64::from(spill_base) + i64::from(slot) * 8;
    emit_mem_q(words, true, vd, SP, off);
}

/// Stores a 16-byte SIMD vector spill slot pair at `sp + spill_base + slot * 8`.
#[inline]
fn emit_st_spill_vec(words: &mut Vec<u32, Alloc<'_>>, vs: Reg, spill_base: u32, slot: u32) {
    let off = i64::from(spill_base) + i64::from(slot) * 8;
    emit_mem_q(words, false, vs, SP, off);
}

/// Resolves an arbitrary simultaneous parallel copy `[(dst, src)]` over registers and spill slots,
/// using `X16`/`X17` for GPR (`FP = false`) and `V31`/`V30` for FP/SIMD (`FP = true`) as cycle
/// breaker and spill-to-spill transfer scratch registers respectively.
fn emit_parallel_moves<'a, const FP: bool>(
    words: &mut Vec<u32, Alloc<'a>>,
    moves: &mut Vec<(CopyLoc<FP>, CopyLoc<FP>), Alloc<'_>>,
    spill_base: u32,
) -> Result<(), TierError> {
    let mut i = 0;
    while let Some(&(d, s)) = moves.get(i) {
        if d == s {
            moves.remove_swap(i);
        } else {
            i += 1;
        }
    }

    let (cycle_scratch, spill_scratch) = if FP { (V31, V30) } else { (X16, X17) };
    let emit_mov = |words: &mut Vec<u32, Alloc<'a>>, d: Reg, s: Reg| {
        // For FP/SIMD: `mov vd.16b, vs.16b` (`orr`) preserves all 128 bits for vectors AND low 64/32 bits for scalars.
        words.push(if FP { enc::orr_v(d, s, s) } else { enc::mov(d, s) });
    };
    let emit_ld = |words: &mut Vec<u32, Alloc<'a>>, d: Reg, s: u32| {
        if FP {
            emit_ld_spill_fp(words, d, spill_base, s);
        } else {
            emit_ld_spill_gpr(words, d, spill_base, s);
        }
    };
    let emit_st = |words: &mut Vec<u32, Alloc<'a>>, s: Reg, d: u32| {
        if FP {
            emit_st_spill_fp(words, s, spill_base, d);
        } else {
            emit_st_spill_gpr(words, s, spill_base, d);
        }
    };
    let emit_one = |words: &mut Vec<u32, Alloc<'a>>, dst: CopyLoc<FP>, src: CopyLoc<FP>| -> Result<(), TierError> {
        match (dst, src) {
            (CopyLoc::Reg(d), CopyLoc::Reg(s)) => {
                if d != s {
                    emit_mov(words, d, s);
                }
            }
            (CopyLoc::Reg(d), CopyLoc::Spill(s)) => emit_ld(words, d, s),
            (CopyLoc::Spill(d), CopyLoc::Reg(s)) => emit_st(words, s, d),
            (CopyLoc::Spill(d), CopyLoc::Spill(s)) => {
                emit_ld(words, spill_scratch, s);
                emit_st(words, spill_scratch, d);
            }
            (CopyLoc::Reg(d), CopyLoc::VecSpill(s)) if FP => emit_ld_spill_vec(words, d, spill_base, s),
            (CopyLoc::VecSpill(d), CopyLoc::Reg(s)) if FP => emit_st_spill_vec(words, s, spill_base, d),
            (CopyLoc::VecSpill(d), CopyLoc::VecSpill(s)) if FP => {
                emit_ld_spill_vec(words, V30, spill_base, s);
                emit_st_spill_vec(words, V30, spill_base, d);
            }
            _ => return Err(TierError::Refused { tier: TIER, inst: None, what: "mismatched FP/Vec spill copy" }),
        }
        Ok(())
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
            emit_one(words, dst, src)?;
        } else {
            // Pure cycle: save `moves[0].0` in `cycle_scratch`, redirect all reads of `moves[0].0`
            // to `cycle_scratch`. Note: `emit_one` uses only `spill_scratch` for spill-to-spill,
            // never touching `cycle_scratch`.
            let (dst, src) = moves.remove_swap(0);
            match dst {
                CopyLoc::Reg(d) => emit_mov(words, cycle_scratch, d),
                CopyLoc::Spill(s) => emit_ld(words, cycle_scratch, s),
                CopyLoc::VecSpill(s) => emit_ld_spill_vec(words, cycle_scratch, spill_base, s),
            }
            for m in moves.iter_mut() {
                if m.1 == dst {
                    m.1 = CopyLoc::Reg(cycle_scratch);
                }
            }
            emit_one(words, dst, src)?;
        }
    }
    Ok(())
}

#[inline]
fn emit_parallel_gpr_moves<'a>(
    words: &mut Vec<u32, Alloc<'a>>,
    moves: &mut Vec<(GprCopyLoc, GprCopyLoc), Alloc<'_>>,
    spill_base: u32,
) -> Result<(), TierError> {
    emit_parallel_moves::<false>(words, moves, spill_base)
}

#[inline]
fn emit_parallel_fp_moves<'a>(
    words: &mut Vec<u32, Alloc<'a>>,
    moves: &mut Vec<(FpCopyLoc, FpCopyLoc), Alloc<'_>>,
    spill_base: u32,
) -> Result<(), TierError> {
    emit_parallel_moves::<true>(words, moves, spill_base)
}

/// AAPCS64 + Apple ARM64 parameter placement for a single signature parameter.
#[derive(Clone, Copy, Debug)]
enum ParamPlace {
    Sret,
    Gpr(Reg),
    GprPair(Reg, Reg),
    Fp(Reg),
    Stack(Stacked),
}

/// Computes the exact register or stacked placement for parameter `p`, updating `(gpr_n, fp_n, nsaa)`.
fn place_param(
    p: &blaze_lir::func::ParamRec,
    gpr_n: &mut usize,
    fp_n: &mut usize,
    nsaa: &mut Nsaa,
) -> Option<ParamPlace> {
    if ParamKind::from_byte(p.kind) == Some(ParamKind::Sret) {
        return Some(ParamPlace::Sret);
    }
    let ty = Ty(p.ty);
    if is_fp_ty(ty).is_some() {
        if let Some(&r) = FP_ARG_REGS.get(*fp_n) {
            *fp_n += 1;
            Some(ParamPlace::Fp(r))
        } else {
            nsaa.place(ty).map(ParamPlace::Stack)
        }
    } else if ty == Ty::I128 {
        let aligned = (*gpr_n + 1) & !1;
        if let (Some(&lo), Some(&hi)) = (ARG_REGS.get(aligned), ARG_REGS.get(aligned + 1)) {
            *gpr_n = aligned + 2;
            Some(ParamPlace::GprPair(lo, hi))
        } else {
            *gpr_n = ARG_REGS.len(); // AAPCS64 C.11
            nsaa.place(Ty::I128).map(ParamPlace::Stack)
        }
    } else if let Some(&r) = ARG_REGS.get(*gpr_n) {
        *gpr_n += 1;
        Some(ParamPlace::Gpr(r))
    } else {
        nsaa.place(ty).map(ParamPlace::Stack)
    }
}

fn size_of_stack_bytes(bytes: u32) -> Size {
    match bytes {
        1 => Size::B,
        2 => Size::H,
        4 => Size::W,
        _ => Size::X,
    }
}

/// Validates a C calling-convention signature within `Sota`'s ABI subset
/// (register + stacked arguments, `sret` in `X8`, `byval`, `i128`, and up to 2 return values in `X0:X1`).
fn check_sota_sig(view: &View<'_>, sig_id: SigId, inst: Option<u32>, os: crate::Os) -> Result<(), TierError> {
    let Some(rec) = view.sigs.get(sig_id.index()) else {
        return Err(TierError::Refused { tier: TIER, inst, what: "missing signature" });
    };
    let conv = Conv::from_byte(rec.conv);
    if conv != Some(Conv::C) && conv != Some(Conv::BlazeResult) {
        return Err(TierError::Refused { tier: TIER, inst, what: "unsupported calling convention" });
    }
    if rec.bits & blaze_lir::func::SigRec::VARIADIC != 0 {
        return Err(TierError::Refused { tier: TIER, inst, what: "variadic signature" });
    }
    let sig = view.sig(sig_id);
    if conv == Some(Conv::BlazeResult) && sig.rets.len() != 2 {
        return Err(TierError::Refused { tier: TIER, inst, what: "blaze_result requires 2 return values (val, is_err)" });
    }
    let mut gpr_n = 0usize;
    let mut fp_n = 0usize;
    let mut nsaa = Nsaa::new(os);
    let mut has_sret = false;
    for p in sig.params {
        let pkind = ParamKind::from_byte(p.kind);
        if pkind == Some(ParamKind::Sret) {
            if has_sret || !Ty(p.ty).is_ptr() {
                return Err(TierError::Refused { tier: TIER, inst, what: "invalid sret parameter" });
            }
            has_sret = true;
            continue;
        }
        if pkind == Some(ParamKind::Byval) {
            if !Ty(p.ty).is_ptr() {
                return Err(TierError::Refused { tier: TIER, inst, what: "non-ptr byval parameter" });
            }
            let Some(facts) = view.facts(p) else {
                return Err(TierError::Refused { tier: TIER, inst, what: "byval without size/align" });
            };
            if facts.align > 4 {
                return Err(TierError::Refused { tier: TIER, inst, what: "byval align > 16" });
            }
        } else if pkind != Some(ParamKind::Value) {
            return Err(TierError::Refused { tier: TIER, inst, what: "unsupported parameter kind" });
        }
        let ty = Ty(p.ty);
        if ty != Ty::I128 && (!is_supported_scalar(ty) || ty == Ty::NONE) {
            return Err(TierError::Refused { tier: TIER, inst, what: "unsupported parameter type" });
        }
        if place_param(p, &mut gpr_n, &mut fp_n, &mut nsaa).is_none() {
            return Err(TierError::Refused { tier: TIER, inst, what: "unplaceable stacked parameter" });
        }
    }
    if nsaa.end() >= MOST_AREA {
        return Err(TierError::Refused { tier: TIER, inst, what: "stacked parameters >= 1 MiB" });
    }
    if sig.rets.len() > 2 {
        return Err(TierError::Refused { tier: TIER, inst, what: "return > 2 values" });
    }
    if sig.rets.len() == 2 {
        for r in sig.rets {
            let ty = Ty(r.ty);
            if ParamKind::from_byte(r.kind) != Some(ParamKind::Value) || is_fp_ty(ty).is_some() || !is_supported_scalar(ty) || ty == Ty::NONE {
                return Err(TierError::Refused { tier: TIER, inst, what: "unsupported 2-value return type" });
            }
        }
    } else {
        for r in sig.rets {
            if ParamKind::from_byte(r.kind) != Some(ParamKind::Value) {
                return Err(TierError::Refused { tier: TIER, inst, what: "non-value return" });
            }
            let ty = Ty(r.ty);
            if ty != Ty::I128 && (!is_supported_scalar(ty) || ty == Ty::NONE) {
                return Err(TierError::Refused { tier: TIER, inst, what: "unsupported return type" });
            }
        }
    }
    Ok(())
}

/// Validates that `view` is inside `Sota`'s direct AArch64 fused subset, returning `Err` naming the
/// exact offending instruction so `Sota::with_fallback` can cleanly delegate exotic ops.
fn check_subset(view: &View<'_>, has_lse: bool, os: crate::Os) -> Result<(), TierError> {
    if view.head.ptr_bits != 64 {
        return Err(TierError::Refused { tier: TIER, inst: None, what: "non-64-bit pointers" });
    }
    if view.blocks() == 0 {
        return Err(TierError::Refused { tier: TIER, inst: None, what: "empty function" });
    }
    for b in 0..view.blocks() {
        if view.block_kind(Block(u32_of(b))).is_landing() {
            let params = view.params(Block(u32_of(b)));
            if params.len() != 1 || ty_at(view.tys, params.start) != Ty::PTR {
                return Err(TierError::Refused { tier: TIER, inst: None, what: "landing block param must be single ptr" });
            }
        }
    }
    check_sota_sig(view, SigId(view.head.sig), None, os)?;
    for slot in view.slots {
        if slot.align > MOST_SLOT_ALIGN {
            return Err(TierError::Refused { tier: TIER, inst: None, what: "slot alignment > 4 KiB" });
        }
    }

    for inst in 0..view.len() {
        let ty = ty_at(view.tys, inst);
        let pair_ty = is_gpr_pair_ty(view, ty);
        let vec_ty = is_vec128_ty(ty);
        let mask_ty = is_vec_mask_ty(ty);
        if !is_supported_scalar(ty) && !pair_ty && vec_ty.is_none() && mask_ty.is_none() {
            return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unsupported value type" });
        }
        let op = view.op(inst);
        let flags = Flags(at_u8(view.flags, inst));
        let honoured = match op {
            Op::Load | Op::Store => Flags::VOLATILE.0,
            Op::AtomicLoad | Op::AtomicStore | Op::AtomicRmw | Op::Cmpxchg | Op::Fence => 0xff,
            _ => 0,
        };
        if op.info().flags.meanings_only(flags).0 & !honoured != 0 {
            return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unhonoured meaning flag" });
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
                | Op::Project
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
                | Op::Rotl
                | Op::Rotr
                | Op::Umulhi
                | Op::Smulhi
                | Op::Umin
                | Op::Umax
                | Op::Smin
                | Op::Smax
                | Op::UaddSat
                | Op::UsubSat
                | Op::SaddSat
                | Op::SsubSat
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
                | Op::Frem
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
                | Op::Memcpy
                | Op::Memmove
                | Op::Memset
                | Op::Memcmp
                | Op::AtomicLoad
                | Op::AtomicStore
                | Op::AtomicRmw
                | Op::Cmpxchg
                | Op::Fence
                | Op::TargetIntrinsic
                | Op::DataAddr
                | Op::FuncAddr
                | Op::TlsAddr
                | Op::Jump
                | Op::Br
                | Op::Switch
                | Op::Return
                | Op::Trap
                | Op::Unreachable
                | Op::Splat
                | Op::Insert
                | Op::Shuffle
                | Op::ReduceAdd
                | Op::ReduceAnd
                | Op::ReduceOr
                | Op::ReduceXor
                | Op::ReduceUmin
                | Op::ReduceUmax
                | Op::ReduceSmin
                | Op::ReduceSmax
                | Op::ReduceFadd
                | Op::Call
                | Op::CallIndirect
                | Op::Invoke
                | Op::InvokeIndirect
                | Op::Resume
        );
        if !ok {
            return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: op.name() });
        }

        // Validate 128-bit NEON vector ops & vector lane masks (`Icmp` -> `<lanes x i1>`, `Select`).
        if mask_ty.is_some() {
            let mask_ok = if op == Op::Icmp {
                let at = to_idx(at_u32(view.data, inst));
                let a_ty = ty_at(view.tys, to_idx(at_u32(view.extra, at + 1)));
                is_vec128_ty(a_ty).is_some_and(|(_, vfp)| !vfp)
            } else {
                false
            };
            if !mask_ok {
                return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unsupported NEON mask op" });
            }
        }
        if let Some((vsize, vfp)) = vec_ty {
            let vec_ok = match op {
                Op::Param | Op::Load | Op::Splat | Op::Uninit | Op::Freeze => true,
                Op::Add
                | Op::Sub
                | Op::And
                | Op::Or
                | Op::Xor
                | Op::Not
                | Op::UaddSat
                | Op::UsubSat
                | Op::SaddSat
                | Op::SsubSat => !vfp,
                Op::Umin | Op::Umax | Op::Smin | Op::Smax => !vfp && vsize < 3,
                Op::Mul => {
                    if !vfp && vsize < 3 {
                        true
                    } else if !vfp && vsize == 3 {
                        let fits_u32 = |v: u32| -> bool {
                            let vi = to_idx(v);
                            if view.op(vi) == Op::Splat {
                                extract_const_u64(view, to_idx(at_u32(view.data, vi)))
                                    .is_some_and(|c| c <= u64::from(u32::MAX))
                            } else if view.op(vi) == Op::And {
                                let aat = to_idx(at_u32(view.data, vi));
                                let (al, ar) = (to_idx(at_u32(view.extra, aat)), to_idx(at_u32(view.extra, aat + 1)));
                                let is_u32_mask = |m: usize| {
                                    view.op(m) == Op::Splat
                                        && extract_const_u64(view, to_idx(at_u32(view.data, m)))
                                            .is_some_and(|c| c <= u64::from(u32::MAX))
                                };
                                is_u32_mask(al) || is_u32_mask(ar)
                            } else {
                                false
                            }
                        };
                        let at = to_idx(at_u32(view.data, inst));
                        fits_u32(at_u32(view.extra, at)) && fits_u32(at_u32(view.extra, at + 1))
                    } else {
                        false
                    }
                }
                Op::Umulhi if !vfp && vsize == 2 => true,
                Op::Bitcast => is_vec128_ty(ty_at(view.tys, to_idx(at_u32(view.data, inst)))).is_some(),
                Op::Shl | Op::Lshr if !vfp && (vsize == 2 || vsize == 3) => {
                    let at = to_idx(at_u32(view.data, inst));
                    let rhs = to_idx(at_u32(view.extra, at + 1));
                    let max_sh = if vsize == 2 { 32 } else { 64 };
                    view.op(rhs) == Op::Splat
                        && extract_const_u64(view, to_idx(at_u32(view.data, rhs))).is_some_and(|k| k < max_sh)
                }
                Op::Insert if !vfp && (vsize == 2 || vsize == 3) => {
                    let at = to_idx(at_u32(view.data, inst));
                    let max_lane = if vsize == 2 { 4 } else { 2 };
                    at_u32(view.extra, at + 2) < max_lane
                }
                Op::Fadd | Op::Fsub | Op::Fmul => vfp,
                Op::Shuffle => {
                    if !vfp && vsize == 0 {
                        let at = to_idx(at_u32(view.data, inst));
                        let n = to_idx(at_u32(view.extra, at + 2));
                        n == 16 && (0..16).all(|k| at_u32(view.extra, at + 3 + k) < 16)
                    } else {
                        false
                    }
                }
                Op::Rotl if !vfp && vsize == 3 => true,
                Op::Select => {
                    let at = to_idx(at_u32(view.data, inst));
                    is_vec_mask_ty(ty_at(view.tys, to_idx(at_u32(view.extra, at)))).is_some()
                }
                _ => false,
            };
            if !vec_ok {
                return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unsupported 128-bit NEON vector op" });
            }
        }
        if matches!(
            op,
            Op::ReduceAdd
                | Op::ReduceAnd
                | Op::ReduceOr
                | Op::ReduceXor
                | Op::ReduceUmin
                | Op::ReduceUmax
                | Op::ReduceSmin
                | Op::ReduceSmax
                | Op::ReduceFadd
        ) {
            let src_v = if op == Op::ReduceFadd {
                to_idx(at_u32(view.extra, to_idx(at_u32(view.data, inst)) + 1))
            } else {
                to_idx(at_u32(view.data, inst))
            };
            let Some((_, vfp)) = is_vec128_ty(ty_at(view.tys, src_v)) else {
                return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "non-128-bit vector reduce" });
            };
            if (op == Op::ReduceFadd) != vfp {
                return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "mismatched vector reduce kind" });
            }
        }

        // Pair types: `Ty::I128` supports native 2-GPR arithmetic/bitwise/shifts/casts/mem;
        // compound pairs are produced by overflow ops, cmpxchg & 2-return calls, and consumed by `Project`/`Param`.
        if ty == Ty::I128 {
            let ok_128 = matches!(
                op,
                Op::Param
                    | Op::Iconst
                    | Op::IconstWide
                    | Op::Uninit
                    | Op::Freeze
                    | Op::Add
                    | Op::Sub
                    | Op::Neg
                    | Op::Mul
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
                    | Op::Zext
                    | Op::Sext
                    | Op::Select
                    | Op::Load
                    | Op::Call
                    | Op::CallIndirect
                    | Op::Invoke
                    | Op::InvokeIndirect
                    | Op::UaddSat
                    | Op::UsubSat
                    | Op::SaddSat
                    | Op::SsubSat
            );
            if !ok_128 {
                return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unsupported i128 op" });
            }
            if op == Op::IconstWide && ty.const_words() != 4 {
                return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "non-4-word i128 const" });
            }
            if matches!(op, Op::Zext | Op::Sext) {
                let src_ty = ty_at(view.tys, to_idx(at_u32(view.data, inst)));
                if !(src_ty.is_scalar() && src_ty.is_int() && src_ty.bits(64) <= 64) {
                    return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unsupported widen to i128" });
                }
            }
        } else if pair_ty
            && !matches!(
                op,
                Op::UaddOverflow
                    | Op::SaddOverflow
                    | Op::UsubOverflow
                    | Op::SsubOverflow
                    | Op::UmulOverflow
                    | Op::SmulOverflow
                    | Op::Cmpxchg
                    | Op::Call
                    | Op::CallIndirect
                    | Op::Invoke
                    | Op::InvokeIndirect
                    | Op::Param
            )
        {
            return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "pair type on unsupported op" });
        }
        if op == Op::Cmpxchg {
            let at = to_idx(at_u32(view.data, inst));
            let val_ty = ty_at(view.tys, to_idx(at_u32(view.extra, at + 1)));
            if !has_lse || mem_size_of(val_ty).is_none() || is_fp_ty(val_ty).is_some() {
                return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unsupported cmpxchg" });
            }
        }

        match op {
            Op::Project => {
                let at = to_idx(at_u32(view.data, inst));
                let src = to_idx(at_u32(view.extra, at));
                let field_idx = at_u32(view.extra, at + 1);
                let src_ty = ty_at(view.tys, src);
                if src_ty.compound_row().is_none() || !is_gpr_pair_ty(view, src_ty) || field_idx >= 2 {
                    return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "non-pair project" });
                }
            }
            Op::UaddOverflow
            | Op::SaddOverflow
            | Op::UsubOverflow
            | Op::SsubOverflow
            | Op::UmulOverflow
            | Op::SmulOverflow => {
                let at = to_idx(at_u32(view.data, inst));
                let a_ty = ty_at(view.tys, to_idx(at_u32(view.extra, at)));
                if !pair_ty || !(a_ty.is_scalar() && a_ty.is_int() && a_ty.bits(64) <= 64) {
                    return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "wide/non-pair overflow" });
                }
            }
            Op::AtomicLoad => {
                if mem_size_of(ty).is_none() || is_fp_ty(ty).is_some() {
                    return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unsupported atomic_load type" });
                }
            }
            Op::AtomicStore => {
                let at = to_idx(at_u32(view.data, inst));
                let val_ty = ty_at(view.tys, to_idx(at_u32(view.extra, at)));
                if mem_size_of(val_ty).is_none() || is_fp_ty(val_ty).is_some() {
                    return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unsupported atomic_store type" });
                }
            }
            Op::AtomicRmw => {
                let at = to_idx(at_u32(view.data, inst));
                let kind = AtomicRmw::from_word(at_u32(view.extra, at));
                if !has_lse || mem_size_of(ty).is_none() || is_fp_ty(ty).is_some() || kind == Some(AtomicRmw::Nand) || kind.is_none() {
                    return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "atomic_rmw requires LSE/supported kind" });
                }
            }
            Op::Memcpy | Op::Memmove | Op::Memset | Op::Memcmp => {
                if flags.contains(Flags::VOLATILE) {
                    return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "volatile bulk mem op" });
                }
            }
            Op::Frem => {
                if is_fp_ty(ty).is_none() {
                    return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "non-f32/f64 frem" });
                }
            }
            Op::TlsAddr => {
                let rec = view.reference(RefId(at_u32(view.data, inst)));
                if RefKind::from_word(rec.kind) != Some(RefKind::Tls) {
                    return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "non-tls tls_addr" });
                }
            }
            Op::TargetIntrinsic => {
                let at = to_idx(at_u32(view.data, inst));
                let name = view.intrinsic_name(IntrinsicId(at_u32(view.extra, at)));
                let argc = to_idx(at_u32(view.extra, at + 1));
                let ok = name == b"blaze.prefetch.r1" && argc == 1 && ty == Ty::NONE;
                if !ok {
                    return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unsupported target_intrinsic" });
                }
            }
            Op::Load => {
                if mem_size_of(ty).is_none() && is_vec128_ty(ty).is_none() && ty != Ty::I128 {
                    return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "load of non-byte width" });
                }
            }
            Op::Store => {
                let at = to_idx(at_u32(view.data, inst));
                let val_ty = ty_at(view.tys, to_idx(at_u32(view.extra, at)));
                if mem_size_of(val_ty).is_none() && is_vec128_ty(val_ty).is_none() && val_ty != Ty::I128 {
                    return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "store of non-byte width" });
                }
            }
            Op::Fconst => {
                if ty != Ty::F32 {
                    return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "non-f32 fconst" });
                }
            }
            Op::FconstWide => {
                if ty != Ty::F64 {
                    return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "non-f64 fconst_wide" });
                }
            }
            Op::FptoSi | Op::FptoUi => {
                let bits = ty.bits(64);
                if bits < 8 || bits > 64 {
                    return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "fptoi to i1" });
                }
            }
            Op::Bitcast => {
                let src_ty = ty_at(view.tys, to_idx(at_u32(view.data, inst)));
                if is_vec128_ty(src_ty).is_some() && is_vec128_ty(ty).is_some() {
                    // 128-bit vector <-> 128-bit vector bitcast is a zero-cost register reinterpretation.
                } else {
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
                            return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unsupported bitcast" });
                        }
                    }
                }
            }
            Op::Trap => {
                if at_u32(view.data, inst) >= BRK_DIV_ZERO {
                    return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "reserved trap code" });
                }
            }
            Op::DataAddr => {
                let rec = view.reference(RefId(at_u32(view.data, inst)));
                match RefKind::from_word(rec.kind) {
                    Some(RefKind::Static | RefKind::Vtable) => {}
                    Some(RefKind::Data) => {
                        let row = rec.place;
                        let Some(&data) = view.datas.get(to_idx(row)) else {
                            return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "missing data row" });
                        };
                        if data.align > u32::from(MOST_DATA_ALIGN) {
                            return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "data align > 4 KiB" });
                        }
                        for r in view.data_relocs_of(DataId(row)) {
                            let target = view.reference(RefId(r.target));
                            if matches!(RefKind::from_word(target.kind), Some(RefKind::Tls) | None) {
                                return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "tls reloc in data" });
                            }
                            if r.offset % 8 != 0 || u64::from(r.offset) + 8 > u64::from(data.bytes_len) {
                                return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "invalid data reloc offset" });
                            }
                        }
                    }
                    _ => {
                        return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unsupported data_addr kind" });
                    }
                }
            }
            Op::FuncAddr => {
                let rec = view.reference(RefId(at_u32(view.data, inst)));
                if RefKind::from_word(rec.kind) != Some(RefKind::Func) {
                    return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "non-func func_addr" });
                }
            }
            Op::Call | Op::CallIndirect => {
                let at = to_idx(at_u32(view.data, inst));
                if op == Op::Call {
                    let rec = view.reference(RefId(at_u32(view.extra, at)));
                    if RefKind::from_word(rec.kind) != Some(RefKind::Func) {
                        return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "non-func call target" });
                    }
                }
                let sig_id = SigId(at_u32(view.extra, at + 1));
                check_sota_sig(view, sig_id, Some(u32_of(inst)), os)?;
                let argc = to_idx(at_u32(view.extra, at + 2));
                if argc != view.sig(sig_id).params.len() {
                    return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "call argc mismatch" });
                }
            }
            Op::Invoke | Op::InvokeIndirect => {
                let at = to_idx(at_u32(view.data, inst));
                if op == Op::Invoke {
                    let rec = view.reference(RefId(at_u32(view.extra, at)));
                    if RefKind::from_word(rec.kind) != Some(RefKind::Func) {
                        return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "non-func invoke target" });
                    }
                }
                let sig_id = SigId(at_u32(view.extra, at + 1));
                check_sota_sig(view, sig_id, Some(u32_of(inst)), os)?;
                let unwind_blk = Block(at_u32(view.extra, at + 3));
                if !view.block_kind(unwind_blk).is_landing() {
                    return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "invoke unwind target not landing" });
                }
                let argc = to_idx(at_u32(view.extra, at + 4));
                if argc != view.sig(sig_id).params.len() {
                    return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "invoke argc mismatch" });
                }
            }
            Op::Resume => {
                let exc_v = to_idx(at_u32(view.data, inst));
                if ty_at(view.tys, exc_v) != Ty::PTR {
                    return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "non-ptr resume operand" });
                }
            }
            Op::Return => {
                let at = to_idx(at_u32(view.data, inst));
                let retc = to_idx(at_u32(view.extra, at));
                if retc != view.sig(SigId(view.head.sig)).rets.len() {
                    return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "return arity mismatch" });
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

/// Resolves a C runtime routine (e.g. `"memcpy"`, `"_Unwind_Resume"`, `"__udivti3"`) into `out.targets`.
fn resolve_c_routine<'a>(name: &str, out: &mut Emitted<'a>) -> u32 {
    let sym = Symbol::of_name(name);
    for (idx, t) in out.targets.iter().enumerate() {
        if t.space == Space::Outside && t.symbol == sym {
            return u32_of(idx);
        }
    }
    let start = u32_of(out.text.len());
    out.text.extend_from_slice(name.as_bytes());
    out.names.push((sym, start, u32_of(name.len())));
    let idx = u32_of(out.targets.len());
    out.targets.push(Target { space: Space::Outside, symbol: sym });
    idx
}

/// Emits a call to a non-unwinding runtime routine (`memcpy`/`memset`/`memcmp`/`fmod`/`__udivti3`).
fn emit_routine_call<'a>(
    words: &mut Vec<u32, Alloc<'a>>,
    target: u32,
    call_mode: CallMode,
    out: &mut Emitted<'a>,
) {
    match call_mode {
        CallMode::DirectBl => {
            let bl_at = u32_of(words.len() * 4);
            words.push(enc::bl(0));
            out.relocs.push(Reloc { at: bl_at, in_relro: false, kind: RelocKind::Branch26, target, addend: 0 });
        }
        CallMode::GotSlot => {
            let adrp_at = u32_of(words.len() * 4);
            words.push(enc::adrp(X16, 0));
            words.push(enc::ldr(Size::X, X16, X16, 0));
            out.relocs.push(Reloc { at: adrp_at, in_relro: false, kind: RelocKind::GotPage21, target, addend: 0 });
            out.relocs.push(Reloc { at: adrp_at + 4, in_relro: false, kind: RelocKind::GotPageOff12, target, addend: 0 });
            words.push(enc::blr(X16));
        }
    }
}

/// Places a `Data` row (and any transitively referenced `Data` rows) into `out.relro`, recording
/// internal `Word` relocations (`Static`/`Vtable`/`Func`/`Data`) and `PositionRow` source-position slots!
fn place_data_row<'a>(
    root_row: u32,
    view: &View<'_>,
    names: &dyn Names,
    data_relro_off: &mut [Option<u32>],
    out: &mut Emitted<'a>,
    inst: usize,
    alloc: Alloc<'a>,
) -> Result<u32, TierError> {
    if let Some(off) = at_opt(data_relro_off, to_idx(root_row)) {
        return Ok(off);
    }
    let mut queue: Vec<u32, Alloc<'a>> = Vec::new_in(alloc);
    let mut pending_data_words: Vec<(u32, u32, i64), Alloc<'a>> = Vec::new_in(alloc);
    queue.push(root_row);
    let mut q_idx = 0usize;
    while let Some(&row) = queue.get(q_idx) {
        q_idx += 1;
        if at_opt(data_relro_off, to_idx(row)).is_some() {
            continue;
        }
        let Some(&data) = view.datas.get(to_idx(row)) else {
            return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "missing data row" });
        };
        if data.align > u32::from(MOST_DATA_ALIGN) {
            return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "data align > 4 KiB" });
        }
        let align = 8usize.max(1 << data.align);
        out.relro_align = out.relro_align.max(u8_of(data.align));
        while out.relro.len() % align != 0 {
            out.relro.push(0);
        }
        let start = u32_of(out.relro.len());
        let bytes = view.data_bytes_of(DataId(row));
        if bytes.is_empty() {
            out.relro.extend_from_slice(&[0; 8]);
        } else {
            out.relro.extend_from_slice(bytes);
        }
        set_at(data_relro_off, to_idx(row), Some(start));

        for p in view.data_positions_of(DataId(row)) {
            let field = match blaze_lir::PositionField::from_word(p.field) {
                Some(blaze_lir::PositionField::Line) => PositionField::Line,
                Some(blaze_lir::PositionField::Col) => PositionField::Col,
                None => {
                    return Err(TierError::Refused {
                        tier: TIER,
                        inst: Some(u32_of(inst)),
                        what: "unknown data position field",
                    });
                }
            };
            let at = start + p.offset;
            let slot_end = to_idx(at).checked_add(4).unwrap_or(usize::MAX);
            if slot_end > out.relro.len() {
                return Err(TierError::Refused {
                    tier: TIER,
                    inst: Some(u32_of(inst)),
                    what: "data position past row end",
                });
            }
            if let Some(slot) = out.relro.get_mut(to_idx(at)..slot_end) {
                slot.fill(0);
            }
            out.position_slots.push(PositionRow {
                part: PositionPart::Relro,
                at,
                item: p.item_key(),
                site: p.site,
                field,
            });
        }

        for r in view.data_relocs_of(DataId(row)) {
            if r.offset % 8 != 0 || u64::from(r.offset) + 8 > u64::from(data.bytes_len) {
                return Err(TierError::Refused {
                    tier: TIER,
                    inst: Some(u32_of(inst)),
                    what: "invalid data reloc offset",
                });
            }
            let word = start + r.offset;
            if let Some(slot) = out.relro.get_mut(to_idx(word)..to_idx(word) + 8) {
                slot.fill(0);
            }
            let target_ref = view.reference(RefId(r.target));
            match RefKind::from_word(target_ref.kind) {
                Some(RefKind::Static) => {
                    let t = resolve_outside(names, Outside::Static(target_ref.place), Space::Static, true, out, inst)?;
                    out.words.push(Word { at: word, target: t, addend: r.addend });
                }
                Some(RefKind::Vtable) => {
                    let t = resolve_outside(names, Outside::Vtable(target_ref.place), Space::Vtable, false, out, inst)?;
                    out.words.push(Word { at: word, target: t, addend: r.addend });
                }
                Some(RefKind::Func) => {
                    let t = resolve_outside(names, Outside::Func(target_ref.place), Space::Func, true, out, inst)?;
                    out.words.push(Word { at: word, target: t, addend: r.addend });
                }
                Some(RefKind::Data) => {
                    if at_opt(data_relro_off, to_idx(target_ref.place)).is_none() {
                        queue.push(target_ref.place);
                    }
                    pending_data_words.push((word, target_ref.place, r.addend));
                }
                Some(RefKind::Tls) | None => {
                    return Err(TierError::Refused {
                        tier: TIER,
                        inst: Some(u32_of(inst)),
                        what: "tls reloc in data",
                    });
                }
            }
        }
    }
    for &(word, target_row, addend) in pending_data_words.iter() {
        let off = at_opt(data_relro_off, to_idx(target_row)).ok_or(TierError::Refused {
            tier: TIER,
            inst: Some(u32_of(inst)),
            what: "missing data relro offset",
        })?;
        out.words.push(Word { at: word, target: OWN_RELRO, addend: i64::from(off) + addend });
    }
    at_opt(data_relro_off, to_idx(root_row)).ok_or(TierError::Refused {
        tier: TIER,
        inst: Some(u32_of(inst)),
        what: "missing root data relro offset",
    })
}

/// Compiles verified, optimized `view` directly into fused AArch64 machine code in an `Emitted`.
pub(crate) fn emit_sota_a64<'a>(
    view: View<'_>,
    names: &dyn Names,
    call_mode: CallMode,
    features: u64,
    is_macos: bool,
    alloc: Alloc<'a>,
) -> Result<Emitted<'a>, TierError> {
    let (emitted, _clobbers) = emit_sota_a64_with_ipra(
        view,
        names,
        call_mode,
        features,
        is_macos,
        |_| CallClobbers::C_ABI_ARM64,
        alloc,
    )?;
    Ok(emitted)
}

/// IPRA-enabled SOTA AArch64 emitter: accepts per-callsite `call_clobbers` summaries so callers
/// preserve live values in untouched caller-saved registers across internal calls with zero spills,
/// and returns this function's exact [`CallClobbers`] summary for downstream callers!
pub(crate) fn emit_sota_a64_with_ipra<'a>(
    view: View<'_>,
    names: &dyn Names,
    call_mode: CallMode,
    features: u64,
    is_macos: bool,
    call_clobbers: impl Fn(usize) -> CallClobbers,
    alloc: Alloc<'a>,
) -> Result<(Emitted<'a>, CallClobbers), TierError> {
    let has_lse = features & blaze_tier::Machine::LSE != 0;
    let has_cssc = features & blaze_tier::Machine::CSSC != 0;
    let has_sha3 = features & blaze_tier::Machine::SHA3 != 0;
    let os = if is_macos { crate::Os::MacOs } else { crate::Os::Linux };
    check_subset(&view, has_lse, os)?;

    let len = view.len();
    let blocks = view.blocks();
    let tail_calls = detect_tail_calls(&view, call_mode == CallMode::DirectBl, alloc);
    let block_order = compute_block_order(&view, alloc);

    // 0. Compute outgoing stacked-arg area (`[sp, #0..out_area]`), outgoing `byval` scratch area
    //    (`[sp, #byval_base..byval_end]`), and local LIR stack slots (`[sp, #byval_end..spill_base]`)
    //    once upfront so `sp` NEVER moves during calls or landing-pad entries!
    let mut max_out_area: u64 = 0;
    let mut max_byval_bytes: u64 = 0;
    for i in 0..len {
        let op = view.op(i);
        if matches!(op, Op::Call | Op::CallIndirect | Op::Invoke | Op::InvokeIndirect) {
            let at = to_idx(at_u32(view.data, i));
            let sig = view.sig(SigId(at_u32(view.extra, at + 1)));
            let mut gpr_n = 0usize;
            let mut fp_n = 0usize;
            let mut nsaa = Nsaa::new(os);
            let mut byval_cur = 0u64;
            for p in sig.params {
                if ParamKind::from_byte(p.kind) == Some(ParamKind::Byval) {
                    let Some(facts) = view.facts(p) else {
                        return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(i)), what: "byval without size/align" });
                    };
                    let align = 1u64 << facts.align;
                    byval_cur = byval_cur.next_multiple_of(align);
                    byval_cur += u64::from(facts.size);
                }
                let _ = place_param(p, &mut gpr_n, &mut fp_n, &mut nsaa);
            }
            max_out_area = max_out_area.max(u64::from(nsaa.end()));
            max_byval_bytes = max_byval_bytes.max(byval_cur);
        }
    }
    let out_area = max_out_area.next_multiple_of(16);
    let byval_base = out_area;
    let byval_size = max_byval_bytes.next_multiple_of(16);

    let mut has_stacked_in = false;
    {
        let mut gpr_n = 0usize;
        let mut fp_n = 0usize;
        let mut nsaa = Nsaa::new(os);
        for p in view.sig(SigId(view.head.sig)).params {
            if matches!(place_param(p, &mut gpr_n, &mut fp_n, &mut nsaa), Some(ParamPlace::Stack(_))) {
                has_stacked_in = true;
            }
        }
    }

    let mut most_align: u64 = 16;
    let mut slot_cursor: u64 = byval_base + byval_size;
    let mut slot_at: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(view.slots.len(), alloc);
    for slot in view.slots {
        let align = 1u64 << slot.align;
        most_align = most_align.max(align);
        slot_cursor = slot_cursor.next_multiple_of(align);
        if slot_cursor >= FRAME_LIMIT {
            return Err(TierError::Refused { tier: TIER, inst: None, what: "stack frame >= 16 MiB" });
        }
        slot_at.push(u32_of(slot_cursor));
        slot_cursor += u64::from(slot.size);
        if slot_cursor >= FRAME_LIMIT {
            return Err(TierError::Refused { tier: TIER, inst: None, what: "stack frame >= 16 MiB" });
        }
    }
    let spill_base_u64 = slot_cursor.next_multiple_of(8);
    if spill_base_u64 >= FRAME_LIMIT {
        return Err(TierError::Refused { tier: TIER, inst: None, what: "stack frame >= 16 MiB" });
    }
    let spill_base = u32_of(spill_base_u64);
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
        for inst in view.block(Block(u32_of(b))) {
            set_at(&mut inst_block, inst, u32_of(b));
            regalloc::for_each_use(&view, inst, |v| {
                let vi = to_idx(v);
                if vi < len {
                    inc_sat_u32(&mut use_count, vi);
                }
            });
        }
    }

    // 2a. ISLE-style pattern fusion detection (sets `fusion[inst]` and marks fused producers in `skip_emit`).
    let mut fusion: Vec<Fusion, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    let mut skip_emit: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    let mut slot_addr_folded_uses: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    let mut blaze_result_br_fused: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    for _ in 0..len {
        fusion.push(Fusion::None);
        skip_emit.push(false);
        slot_addr_folded_uses.push(0);
        blaze_result_br_fused.push(false);
    }

    for inst in 0..len {
        let op = view.op(inst);
        let ty = ty_at(view.tys, inst);
        let bits = if ty == Ty::NONE
            || ty == Ty::I128
            || is_fp_ty(ty).is_some()
            || is_vec128_ty(ty).is_some()
            || is_vec_mask_ty(ty).is_some()
            || ty.compound_row().is_some()
        {
            0
        } else {
            ty.bits(64)
        };
        let b = at_u32(&inst_block, inst);

        match op {
            Op::Add if bits == 64 => {
                let at = to_idx(at_u32(view.data, inst));
                let lhs = at_u32(view.extra, at);
                let rhs = at_u32(view.extra, at + 1);
                // Pattern 1: add(mul(a, b), c) or add(c, mul(a, b)) -> madd
                let try_mul = |prod: u32, acc: u32| -> Option<(u32, u32, u32)> {
                    let pi = to_idx(prod);
                    if at_u32(&use_count, pi) == 1 && at_u32(&inst_block, pi) == b && view.op(pi) == Op::Mul && !at_bool(&skip_emit, pi) {
                        let pat = to_idx(at_u32(view.data, pi));
                        Some((at_u32(view.extra, pat), at_u32(view.extra, pat + 1), acc))
                    } else {
                        None
                    }
                };
                if let Some((a, mb, acc)) = try_mul(lhs, rhs) {
                    set_at(&mut fusion, inst, Fusion::Madd { a, b: mb, acc });
                    set_at(&mut skip_emit, to_idx(lhs), true);
                    continue;
                }
                if let Some((a, mb, acc)) = try_mul(rhs, lhs) {
                    set_at(&mut fusion, inst, Fusion::Madd { a, b: mb, acc });
                    set_at(&mut skip_emit, to_idx(rhs), true);
                    continue;
                }
                // Pattern 2: add(a, shl(b, k)) / add(a, lshr(b, k)) -> add_lsl / add_lsr
                let try_shift = |sh: u32, other: u32| -> Option<(Op, u32, u32, u32, u32)> {
                    let si = to_idx(sh);
                    let sop = view.op(si);
                    if at_u32(&use_count, si) == 1
                        && at_u32(&inst_block, si) == b
                        && matches!(sop, Op::Shl | Op::Lshr)
                        && !at_bool(&skip_emit, si)
                    {
                        let sat = to_idx(at_u32(view.data, si));
                        let sb = at_u32(view.extra, sat);
                        let sk = at_u32(view.extra, sat + 1);
                        if let Some(k) = at_opt(&const_u64, to_idx(sk)) {
                            return Some((sop, other, sb, trunc_u32(k & 63), sk));
                        }
                    }
                    None
                };
                if let Some((sop, a, sb, shift, sk)) = try_shift(rhs, lhs) {
                    set_at(&mut fusion, inst, if sop == Op::Shl {
                        Fusion::AddLsl { a, b: sb, shift, imm_inst: sk }
                    } else {
                        Fusion::AddLsr { a, b: sb, shift, imm_inst: sk }
                    });
                    set_at(&mut skip_emit, to_idx(rhs), true);
                    continue;
                }
                if let Some((sop, a, sb, shift, sk)) = try_shift(lhs, rhs) {
                    set_at(&mut fusion, inst, if sop == Op::Shl {
                        Fusion::AddLsl { a, b: sb, shift, imm_inst: sk }
                    } else {
                        Fusion::AddLsr { a, b: sb, shift, imm_inst: sk }
                    });
                    set_at(&mut skip_emit, to_idx(lhs), true);
                    continue;
                }
                // Pattern 3: `U128`/`U256` hardware carry-chain final limb:
                // `add(add(a, b), zext(carry))` where `carry` comes directly from an earlier
                // `uadd_overflow` (or `AdcsOverflowChain`) in the same block with no intervening flag clobber!
                let try_adcs = |sum_v: u32, carry_zext: u32| -> Option<(u32, u32, u32, usize, usize)> {
                    let si = to_idx(sum_v);
                    let zi = to_idx(carry_zext);
                    if at_u32(&use_count, si) != 1
                        || at_u32(&inst_block, si) != b
                        || view.op(si) != Op::Add
                        || at_fusion(&fusion, si) != Fusion::None
                        || at_bool(&skip_emit, si)
                    {
                        return None;
                    }
                    if at_u32(&use_count, zi) != 1
                        || at_u32(&inst_block, zi) != b
                        || view.op(zi) != Op::Zext
                        || at_bool(&skip_emit, zi)
                    {
                        return None;
                    }
                    let c_i1 = to_idx(at_u32(view.data, zi));
                    if at_u32(&inst_block, c_i1) != b || ty_at(view.tys, c_i1) != Ty::I1 {
                        return None;
                    }
                    // Identify the flag-producing instruction (`UaddOverflow` 64-bit or `AdcsOverflowChain` via `Or`).
                    let flag_setter = if view.op(c_i1) == Op::Project {
                        let pat = to_idx(at_u32(view.data, c_i1));
                        let src = to_idx(at_u32(view.extra, pat));
                        let fidx = at_u32(view.extra, pat + 1);
                        if fidx == 1
                            && at_u32(&inst_block, src) == b
                            && view.op(src) == Op::UaddOverflow
                            && ty_at(view.tys, to_idx(at_u32(view.extra, to_idx(at_u32(view.data, src))))).bits(64) == 64
                        {
                            src
                        } else {
                            return None;
                        }
                    } else if view.op(c_i1) == Op::Or && at_bool(&skip_emit, c_i1) {
                        // Already fused as `carry_out_or` of an `AdcsOverflowChain`!
                        let mut found = None;
                        for k in (0..inst).rev() {
                            if let Fusion::AdcsOverflowChain { carry_out_or: Some(co), .. } = at_fusion(&fusion, k) {
                                if to_idx(co) == c_i1 {
                                    found = Some(k);
                                    break;
                                }
                            }
                        }
                        found?
                    } else {
                        return None;
                    };
                    // Ensure no non-skipped instruction in `(flag_setter + 1)..inst` clobbers ARM64 NZCV flags!
                    for k in (flag_setter + 1)..inst {
                        if at_bool(&skip_emit, k) || k == si || k == zi || k == c_i1 {
                            continue;
                        }
                        let kop = view.op(k);
                        if matches!(
                            kop,
                            Op::Icmp
                                | Op::Fcmp
                                | Op::Select
                                | Op::UaddOverflow
                                | Op::SaddOverflow
                                | Op::UsubOverflow
                                | Op::SsubOverflow
                                | Op::UmulOverflow
                                | Op::SmulOverflow
                                | Op::UaddSat
                                | Op::UsubSat
                                | Op::SaddSat
                                | Op::SsubSat
                                | Op::Umin
                                | Op::Umax
                                | Op::Smin
                                | Op::Smax
                                | Op::Call
                                | Op::CallIndirect
                                | Op::Udiv
                                | Op::Sdiv
                                | Op::Urem
                                | Op::Srem
                                | Op::Memcpy
                                | Op::Memset
                                | Op::AtomicRmw
                                | Op::Br
                                | Op::Switch
                        ) {
                            return None;
                        }
                    }
                    let sat = to_idx(at_u32(view.data, si));
                    Some((at_u32(view.extra, sat), at_u32(view.extra, sat + 1), u32_of(flag_setter), si, zi))
                };
                if let Some((a, mb, csrc, si, zi)) = try_adcs(lhs, rhs).or_else(|| try_adcs(rhs, lhs)) {
                    set_at(&mut fusion, inst, Fusion::AdcsChain { a, b: mb, carry_src: csrc });
                    set_at(&mut skip_emit, si, true);
                    set_at(&mut skip_emit, zi, true);
                    continue;
                }
            }
            Op::UaddOverflow if ty_at(view.tys, to_idx(at_u32(view.extra, to_idx(at_u32(view.data, inst))))).bits(64) == 64 => {
                // Multi-limb `U256` intermediate carry-chain limb:
                //   p1 = uadd_overflow(a, b); s1 = proj(p1, 0); c1 = proj(p1, 1);
                //   inst = uadd_overflow(s1, zext(prev_carry));
                //   cout = or(c1, proj(inst, 1))
                // -> single hardware `adcs rd_lo, ra, rb` keeping ARM64 NZCV `C` live!
                let at = to_idx(at_u32(view.data, inst));
                let (lhs, rhs) = (at_u32(view.extra, at), at_u32(view.extra, at + 1));
                let try_mid_limb = |s1_v: u32, zext_v: u32| -> Option<(u32, u32, u32, usize, usize, usize, usize, Option<u32>)> {
                    let s1i = to_idx(s1_v);
                    let zi = to_idx(zext_v);
                    if at_u32(&use_count, s1i) != 1 || at_u32(&inst_block, s1i) != b || view.op(s1i) != Op::Project || at_bool(&skip_emit, s1i) {
                        return None;
                    }
                    let p1_at = to_idx(at_u32(view.data, s1i));
                    let p1 = to_idx(at_u32(view.extra, p1_at));
                    if at_u32(view.extra, p1_at + 1) != 0
                        || at_u32(&inst_block, p1) != b
                        || view.op(p1) != Op::UaddOverflow
                        || at_bool(&skip_emit, p1)
                        || at_fusion(&fusion, p1) != Fusion::None
                    {
                        return None;
                    }
                    if at_u32(&use_count, zi) != 1 || at_u32(&inst_block, zi) != b || view.op(zi) != Op::Zext || at_bool(&skip_emit, zi) {
                        return None;
                    }
                    let c_prev = to_idx(at_u32(view.data, zi));
                    if at_u32(&inst_block, c_prev) != b || ty_at(view.tys, c_prev) != Ty::I1 {
                        return None;
                    }
                    let flag_setter = if view.op(c_prev) == Op::Project {
                        let cpat = to_idx(at_u32(view.data, c_prev));
                        let src = to_idx(at_u32(view.extra, cpat));
                        if at_u32(view.extra, cpat + 1) == 1
                            && at_u32(&inst_block, src) == b
                            && view.op(src) == Op::UaddOverflow
                            && src != p1
                            && ty_at(view.tys, to_idx(at_u32(view.extra, to_idx(at_u32(view.data, src))))).bits(64) == 64
                        {
                            src
                        } else {
                            return None;
                        }
                    } else if view.op(c_prev) == Op::Or && at_bool(&skip_emit, c_prev) {
                        let mut found = None;
                        for k in (0..inst).rev() {
                            if let Fusion::AdcsOverflowChain { carry_out_or: Some(co), .. } = at_fusion(&fusion, k) {
                                if to_idx(co) == c_prev {
                                    found = Some(k);
                                    break;
                                }
                            }
                        }
                        found?
                    } else {
                        return None;
                    };
                    // Find `c1 = proj(p1, 1)` and `cout = or(c1, proj(inst, 1))` (if `p1` has 2 uses: `s1` and `c1`).
                    if at_u32(&use_count, p1) != 2 {
                        return None;
                    }
                    let mut c1_opt = None;
                    for k in (p1 + 1)..view.block(Block(b)).end {
                        if view.op(k) == Op::Project {
                            let kat = to_idx(at_u32(view.data, k));
                            if to_idx(at_u32(view.extra, kat)) == p1 && at_u32(view.extra, kat + 1) == 1 {
                                c1_opt = Some(k);
                                break;
                            }
                        }
                    }
                    let c1 = c1_opt?;
                    if at_u32(&use_count, c1) != 1 {
                        return None;
                    }
                    // Find `c2 = proj(inst, 1)` and `cout = or(c1, c2)`.
                    let mut cout_opt = None;
                    for k in (inst + 1)..view.block(Block(b)).end {
                        if view.op(k) == Op::Or && ty_at(view.tys, k) == Ty::I1 {
                            let oat = to_idx(at_u32(view.data, k));
                            let (ol, or_) = (to_idx(at_u32(view.extra, oat)), to_idx(at_u32(view.extra, oat + 1)));
                            let other = if ol == c1 {
                                or_
                            } else if or_ == c1 {
                                ol
                            } else {
                                continue;
                            };
                            if view.op(other) == Op::Project {
                                let opat = to_idx(at_u32(view.data, other));
                                if to_idx(at_u32(view.extra, opat)) == inst && at_u32(view.extra, opat + 1) == 1 {
                                    cout_opt = Some(u32_of(k));
                                    break;
                                }
                            }
                        }
                    }
                    let cout = cout_opt?;
                    // Ensure no intervening non-skipped instruction between `flag_setter` and `inst` clobbers NZCV!
                    for k in (flag_setter + 1)..inst {
                        if at_bool(&skip_emit, k) || k == p1 || k == s1i || k == c1 || k == zi || k == c_prev {
                            continue;
                        }
                        let kop = view.op(k);
                        if matches!(
                            kop,
                            Op::Icmp
                                | Op::Fcmp
                                | Op::Select
                                | Op::UaddOverflow
                                | Op::SaddOverflow
                                | Op::UsubOverflow
                                | Op::SsubOverflow
                                | Op::UmulOverflow
                                | Op::SmulOverflow
                                | Op::UaddSat
                                | Op::UsubSat
                                | Op::SaddSat
                                | Op::SsubSat
                                | Op::Umin
                                | Op::Umax
                                | Op::Smin
                                | Op::Smax
                                | Op::Call
                                | Op::CallIndirect
                                | Op::Udiv
                                | Op::Sdiv
                                | Op::Urem
                                | Op::Srem
                                | Op::Memcpy
                                | Op::Memset
                                | Op::AtomicRmw
                                | Op::Br
                                | Op::Switch
                        ) {
                            return None;
                        }
                    }
                    let p1_data = to_idx(at_u32(view.data, p1));
                    Some((at_u32(view.extra, p1_data), at_u32(view.extra, p1_data + 1), u32_of(flag_setter), p1, s1i, c1, zi, Some(cout)))
                };
                if let Some((a, mb, csrc, p1, s1i, c1, zi, cout)) = try_mid_limb(lhs, rhs).or_else(|| try_mid_limb(rhs, lhs)) {
                    set_at(&mut fusion, inst, Fusion::AdcsOverflowChain { a, b: mb, carry_src: csrc, carry_out_or: cout });
                    set_at(&mut skip_emit, p1, true);
                    set_at(&mut skip_emit, s1i, true);
                    set_at(&mut skip_emit, c1, true);
                    set_at(&mut skip_emit, zi, true);
                    if let Some(co) = cout {
                        set_at(&mut skip_emit, to_idx(co), true);
                    }
                    continue;
                }
            }
            Op::Add if is_vec128_ty(ty) == Some((3, false)) => {
                // Fuse `add(add(acc, and(bitcast(e: <4 x i32>), splat(0xffff_ffff))), lshr(bitcast(e), splat(32)))`
                // -> single-instruction `uadalp.2d vacc, ve.4s`!
                let at = to_idx(at_u32(view.data, inst));
                let (lhs, rhs) = (at_u32(view.extra, at), at_u32(view.extra, at + 1));
                let is_splat_val = |v: u32, want: u64| -> bool {
                    let vi = to_idx(v);
                    view.op(vi) == Op::Splat && at_opt(&const_u64, to_idx(at_u32(view.data, vi))) == Some(want)
                };
                let try_uadalp = |inner_v: u32, hi_v: u32| -> Option<(u32, u32, usize, usize, usize, usize)> {
                    let ii = to_idx(inner_v);
                    let hi = to_idx(hi_v);
                    if at_u32(&use_count, ii) != 1 || at_u32(&use_count, hi) != 1 || at_u32(&inst_block, ii) != b || at_u32(&inst_block, hi) != b {
                        return None;
                    }
                    if view.op(ii) != Op::Add || view.op(hi) != Op::Lshr || at_bool(&skip_emit, ii) || at_bool(&skip_emit, hi) || at_fusion(&fusion, ii) != Fusion::None {
                        return None;
                    }
                    let hat = to_idx(at_u32(view.data, hi));
                    let (bc_v, sh_v) = (at_u32(view.extra, hat), at_u32(view.extra, hat + 1));
                    if !is_splat_val(sh_v, 32) {
                        return None;
                    }
                    let bci = to_idx(bc_v);
                    if at_u32(&use_count, bci) != 2 || at_u32(&inst_block, bci) != b || view.op(bci) != Op::Bitcast || at_bool(&skip_emit, bci) {
                        return None;
                    }
                    let src_4s = at_u32(view.data, bci);
                    if is_vec128_ty(ty_at(view.tys, to_idx(src_4s))) != Some((2, false)) {
                        return None;
                    }
                    let iat = to_idx(at_u32(view.data, ii));
                    let (ia, ib) = (at_u32(view.extra, iat), at_u32(view.extra, iat + 1));
                    let check_lo = |lo_v: u32, acc_v: u32| -> Option<(u32, usize)> {
                        let li = to_idx(lo_v);
                        if at_u32(&use_count, li) != 1 || at_u32(&inst_block, li) != b || view.op(li) != Op::And || at_bool(&skip_emit, li) {
                            return None;
                        }
                        let lat = to_idx(at_u32(view.data, li));
                        let (la, lb) = (at_u32(view.extra, lat), at_u32(view.extra, lat + 1));
                        if (la == bc_v && is_splat_val(lb, u64::from(u32::MAX)))
                            || (lb == bc_v && is_splat_val(la, u64::from(u32::MAX)))
                        {
                            Some((acc_v, li))
                        } else {
                            None
                        }
                    };
                    let (acc, li) = check_lo(ib, ia).or_else(|| check_lo(ia, ib))?;
                    Some((acc, src_4s, ii, li, hi, bci))
                };
                if let Some((acc, src_4s, ii, li, hi, bci)) =
                    try_uadalp(lhs, rhs).or_else(|| try_uadalp(rhs, lhs))
                {
                    set_at(&mut fusion, inst, Fusion::VecUadalp { acc, src_4s });
                    set_at(&mut skip_emit, ii, true);
                    set_at(&mut skip_emit, li, true);
                    set_at(&mut skip_emit, hi, true);
                    set_at(&mut skip_emit, bci, true);
                    continue;
                }
            }
            Op::Sub if bits == 64 => {
                let at = to_idx(at_u32(view.data, inst));
                let lhs = at_u32(view.extra, at);
                let rhs = at_u32(view.extra, at + 1);
                let ri = to_idx(rhs);
                if at_u32(&use_count, ri) == 1 && at_u32(&inst_block, ri) == b && view.op(ri) == Op::Mul && !at_bool(&skip_emit, ri) {
                    let pat = to_idx(at_u32(view.data, ri));
                    set_at(&mut fusion, inst, Fusion::Msub { a: at_u32(view.extra, pat), b: at_u32(view.extra, pat + 1), acc: lhs });
                    set_at(&mut skip_emit, ri, true);
                    continue;
                }
                if at_u32(&use_count, ri) == 1 && at_u32(&inst_block, ri) == b && view.op(ri) == Op::Shl && !at_bool(&skip_emit, ri) {
                    let sat = to_idx(at_u32(view.data, ri));
                    let sb = at_u32(view.extra, sat);
                    let sk = at_u32(view.extra, sat + 1);
                    if let Some(k) = at_opt(&const_u64, to_idx(sk)) {
                        set_at(&mut fusion, inst, Fusion::SubLsl { a: lhs, b: sb, shift: trunc_u32(k & 63), imm_inst: sk });
                        set_at(&mut skip_emit, ri, true);
                        continue;
                    }
                }
            }
            Op::Fadd
                if (is_fp_ty(ty).is_some() && Flags(at_u8(view.flags, inst)).contains(Flags::CONTRACT))
                    || (is_vec128_ty(ty).is_some_and(|(_, fp)| fp)
                        && (Flags(at_u8(view.flags, inst)).contains(Flags::CONTRACT)
                            || Flags(at_u8(view.flags, inst)).contains(Flags::REASSOC))) =>
            {
                let at = to_idx(at_u32(view.data, inst));
                let lhs = at_u32(view.extra, at);
                let rhs = at_u32(view.extra, at + 1);
                let is_vec = is_vec128_ty(ty).is_some();
                let try_fmul = |prod: u32, acc: u32| -> Option<(u32, u32, u32)> {
                    let pi = to_idx(prod);
                    let pflags = Flags(at_u8(view.flags, pi));
                    let ok_flag = pflags.contains(Flags::CONTRACT) || (is_vec && pflags.contains(Flags::REASSOC));
                    if at_u32(&use_count, pi) == 1 && at_u32(&inst_block, pi) == b && view.op(pi) == Op::Fmul && ok_flag && !at_bool(&skip_emit, pi) {
                        let pat = to_idx(at_u32(view.data, pi));
                        Some((at_u32(view.extra, pat), at_u32(view.extra, pat + 1), acc))
                    } else {
                        None
                    }
                };
                if let Some((a, mb, acc)) = try_fmul(lhs, rhs) {
                    set_at(&mut fusion, inst, Fusion::Fmadd { a, b: mb, acc });
                    set_at(&mut skip_emit, to_idx(lhs), true);
                    continue;
                }
                if let Some((a, mb, acc)) = try_fmul(rhs, lhs) {
                    set_at(&mut fusion, inst, Fusion::Fmadd { a, b: mb, acc });
                    set_at(&mut skip_emit, to_idx(rhs), true);
                    continue;
                }
            }
            Op::Fsub if is_fp_ty(ty).is_some() && Flags(at_u8(view.flags, inst)).contains(Flags::CONTRACT) => {
                let at = to_idx(at_u32(view.data, inst));
                let lhs = at_u32(view.extra, at);
                let rhs = at_u32(view.extra, at + 1);
                let ri = to_idx(rhs);
                if at_u32(&use_count, ri) == 1
                    && at_u32(&inst_block, ri) == b
                    && view.op(ri) == Op::Fmul
                    && Flags(at_u8(view.flags, ri)).contains(Flags::CONTRACT)
                    && !at_bool(&skip_emit, ri)
                {
                    let pat = to_idx(at_u32(view.data, ri));
                    set_at(&mut fusion, inst, Fusion::Fmsub { a: at_u32(view.extra, pat), b: at_u32(view.extra, pat + 1), acc: lhs });
                    set_at(&mut skip_emit, ri, true);
                    continue;
                }
            }
            Op::And | Op::Or | Op::Xor if bits == 64 || bits == 32 => {
                let at = to_idx(at_u32(view.data, inst));
                let lhs = at_u32(view.extra, at);
                let rhs = at_u32(view.extra, at + 1);
                let try_not = |cand: u32, other: u32| -> Option<(u32, u32)> {
                    let ni = to_idx(cand);
                    if at_u32(&use_count, ni) == 1 && at_u32(&inst_block, ni) == b && view.op(ni) == Op::Not && !at_bool(&skip_emit, ni) {
                        Some((other, at_u32(view.data, ni)))
                    } else {
                        None
                    }
                };
                if let Some((a, nb)) = try_not(rhs, lhs) {
                    set_at(&mut fusion, inst, match op {
                        Op::And => Fusion::Bic { a, b: nb },
                        Op::Or => Fusion::Orn { a, b: nb },
                        _ => Fusion::Eon { a, b: nb },
                    });
                    set_at(&mut skip_emit, to_idx(rhs), true);
                    continue;
                }
                if let Some((a, nb)) = try_not(lhs, rhs) {
                    set_at(&mut fusion, inst, match op {
                        Op::And => Fusion::Bic { a, b: nb },
                        Op::Or => Fusion::Orn { a, b: nb },
                        _ => Fusion::Eon { a, b: nb },
                    });
                    set_at(&mut skip_emit, to_idx(lhs), true);
                    continue;
                }
                if op == Op::Xor && ty.bits(64) == 64 {
                    let try_xor_shift = |sh: u32, other: u32| -> Option<(Op, u32, u32, u32, u32)> {
                        let si = to_idx(sh);
                        let sop = view.op(si);
                        if at_u32(&use_count, si) == 1
                            && at_u32(&inst_block, si) == b
                            && matches!(sop, Op::Shl | Op::Lshr)
                            && !at_bool(&skip_emit, si)
                        {
                            let sat = to_idx(at_u32(view.data, si));
                            let sb = at_u32(view.extra, sat);
                            let sk = at_u32(view.extra, sat + 1);
                            if let Some(k) = at_opt(&const_u64, to_idx(sk)) {
                                return Some((sop, other, sb, trunc_u32(k & 63), sk));
                            }
                        }
                        None
                    };
                    if let Some((sop, a, sb, shift, sk)) = try_xor_shift(rhs, lhs) {
                        set_at(&mut fusion, inst, if sop == Op::Shl {
                            Fusion::EorLsl { a, b: sb, shift, imm_inst: sk }
                        } else {
                            Fusion::EorLsr { a, b: sb, shift, imm_inst: sk }
                        });
                        set_at(&mut skip_emit, to_idx(rhs), true);
                        continue;
                    }
                    if let Some((sop, a, sb, shift, sk)) = try_xor_shift(lhs, rhs) {
                        set_at(&mut fusion, inst, if sop == Op::Shl {
                            Fusion::EorLsl { a, b: sb, shift, imm_inst: sk }
                        } else {
                            Fusion::EorLsr { a, b: sb, shift, imm_inst: sk }
                        });
                        set_at(&mut skip_emit, to_idx(lhs), true);
                        continue;
                    }
                }
            }
            Op::Load | Op::Store => {
                let at = to_idx(at_u32(view.data, inst));
                let (ptr, off, access_ty) = if op == Op::Load {
                    (at_u32(view.extra, at), i64::from(at_u32(view.extra, at + 1).cast_signed()), ty)
                } else {
                    let val = at_u32(view.extra, at);
                    (at_u32(view.extra, at + 1), i64::from(at_u32(view.extra, at + 2).cast_signed()), ty_at(view.tys, to_idx(val)))
                };
                if access_ty == Ty::I128 {
                    continue;
                }
                let is_vec128 = is_vec128_ty(access_ty).is_some();
                let mem_sz = if is_vec128 {
                    None
                } else {
                    let Some((sz, _)) = mem_size_of(access_ty) else {
                        continue;
                    };
                    Some(sz)
                };
                let size_shift: u32 = mem_sz.map_or(4, mem_shift);
                let fits_off = |o: i64| -> bool {
                    match mem_sz {
                        None => (o >= 0 && o % 16 == 0 && o / 16 < 4096) || (-256..=255).contains(&o),
                        Some(sz) => fits_mem_off(sz, o),
                    }
                };
                let pi = to_idx(ptr);
                let pop = view.op(pi);
                // Pattern A: `load`/`store` on `slot_addr(slot)` -> `[sp, #(slot_at[slot] + off)]`!
                if pop == Op::SlotAddr {
                    let s_off = i64::from(at_u32(&slot_at, to_idx(at_u32(view.data, pi))));
                    let total = s_off + off;
                    if fits_off(total) {
                        set_at(&mut fusion, inst, Fusion::MemSpOff { off: total });
                        inc_u32(&mut slot_addr_folded_uses, pi);
                        continue;
                    }
                }
                // Pattern B: `load`/`store` on single-use same-block `add`/`ptr_add` -> `[base, #imm]` or `[base, idx, lsl #size]`!
                if at_u32(&use_count, pi) == 1 && at_u32(&inst_block, pi) == b && !at_bool(&skip_emit, pi) {
                    if pop == Op::PtrAdd {
                        let pat = to_idx(at_u32(view.data, pi));
                        let base = at_u32(view.extra, pat);
                        let by = at_u32(view.extra, pat + 1);
                        if let Some(cv) = at_opt(&const_u64, to_idx(by)) {
                            let by_bits = ty_at(view.tys, to_idx(by)).bits(64);
                            let delta = sign_extend_u64(cv, by_bits);
                            if let Some(total) = delta.checked_add(off) {
                                if fits_off(total) {
                                    set_at(&mut fusion, inst, Fusion::MemBaseOff { base, off: total, imm_inst: by });
                                    set_at(&mut skip_emit, pi, true);
                                    continue;
                                }
                            }
                        } else if off == 0 && ty_at(view.tys, to_idx(by)).bits(64) == 64 {
                            // Pattern B2: `ptr_add(base, shl(idx, log2(size)))` -> `[base, idx, lsl #size]`!
                            let bi = to_idx(by);
                            if size_shift > 0
                                && at_u32(&use_count, bi) == 1
                                && at_u32(&inst_block, bi) == b
                                && view.op(bi) == Op::Shl
                                && !at_bool(&skip_emit, bi)
                            {
                                let sat = to_idx(at_u32(view.data, bi));
                                let idx_v = at_u32(view.extra, sat);
                                let sh_v = at_u32(view.extra, sat + 1);
                                if at_opt(&const_u64, to_idx(sh_v)) == Some(u64::from(size_shift)) {
                                    set_at(&mut fusion, inst, Fusion::MemScaledIdx { base, idx: idx_v, shift_imm: sh_v });
                                    set_at(&mut skip_emit, pi, true);
                                    set_at(&mut skip_emit, bi, true);
                                    continue;
                                }
                            }
                            set_at(&mut fusion, inst, Fusion::MemRegIdx { base, idx: by });
                            set_at(&mut skip_emit, pi, true);
                            continue;
                        }
                    } else if pop == Op::Add && ty_at(view.tys, pi).bits(64) == 64 {
                        if let Fusion::AddLsl { a: base, b: idx, shift, imm_inst } = at_fusion(&fusion, pi) {
                            if off == 0 && shift == size_shift && shift > 0 {
                                set_at(&mut fusion, inst, Fusion::MemScaledIdx { base, idx, shift_imm: imm_inst });
                                set_at(&mut skip_emit, pi, true);
                                continue;
                            }
                        } else if at_fusion(&fusion, pi) == Fusion::None {
                            let pat = to_idx(at_u32(view.data, pi));
                            let a_lhs = at_u32(view.extra, pat);
                            let a_rhs = at_u32(view.extra, pat + 1);
                            let cand = if let Some(cv) = at_opt(&const_u64, to_idx(a_rhs)) {
                                Some((a_lhs, a_rhs, cv.cast_signed()))
                            } else if let Some(cv) = at_opt(&const_u64, to_idx(a_lhs)) {
                                Some((a_rhs, a_lhs, cv.cast_signed()))
                            } else {
                                None
                            };
                            if let Some((base, imm_inst, delta)) = cand {
                                if let Some(total) = delta.checked_add(off) {
                                    if fits_off(total) {
                                        set_at(&mut fusion, inst, Fusion::MemBaseOff { base, off: total, imm_inst });
                                        set_at(&mut skip_emit, pi, true);
                                        continue;
                                    }
                                }
                            } else if off == 0 {
                                set_at(&mut fusion, inst, Fusion::MemRegIdx { base: a_lhs, idx: a_rhs });
                                set_at(&mut skip_emit, pi, true);
                                continue;
                            }
                        }
                    }
                }
            }
            Op::Xor if has_sha3 && is_vec128_ty(ty).is_some_and(|(_, fp)| !fp) => {
                let at = to_idx(at_u32(view.data, inst));
                let (lhs, rhs) = (at_u32(view.extra, at), at_u32(view.extra, at + 1));
                // 1. ARMv8.2-SHA3 Keccak Chi: `xor(a, and(b, not(c)))` -> 1-instruction `bcax vd.16b, va, vb, vc`!
                let try_bcax = |and_v: u32, a_v: u32| -> Option<(u32, u32, u32, usize, usize)> {
                    let ai = to_idx(and_v);
                    if at_u32(&use_count, ai) != 1 || at_u32(&inst_block, ai) != b || view.op(ai) != Op::And || at_fusion(&fusion, ai) != Fusion::None || at_bool(&skip_emit, ai) {
                        return None;
                    }
                    let aat = to_idx(at_u32(view.data, ai));
                    let (al, ar) = (at_u32(view.extra, aat), at_u32(view.extra, aat + 1));
                    let check_not = |cand: u32, other: u32| -> Option<(u32, u32, u32, usize, usize)> {
                        let ci = to_idx(cand);
                        if at_u32(&use_count, ci) == 1 && at_u32(&inst_block, ci) == b && view.op(ci) == Op::Not && !at_bool(&skip_emit, ci) {
                            return Some((a_v, other, at_u32(view.data, ci), ai, ci));
                        }
                        None
                    };
                    check_not(ar, al).or_else(|| check_not(al, ar))
                };
                if let Some((a, mb, c, ai, ci)) = try_bcax(rhs, lhs).or_else(|| try_bcax(lhs, rhs)) {
                    set_at(&mut fusion, inst, Fusion::Bcax { a, b: mb, c });
                    set_at(&mut skip_emit, ai, true);
                    set_at(&mut skip_emit, ci, true);
                    continue;
                }
                // 2. ARMv8.2-SHA3 Keccak Theta: `xor(a, rotl(b, splat(1)))` on `<2 x i64>` -> `rax1 vd.2d, va, vb`!
                if is_vec128_ty(ty) == Some((3, false)) {
                    let try_rax1 = |rot_v: u32, a_v: u32| -> Option<(u32, u32, usize)> {
                        let ri = to_idx(rot_v);
                        if at_u32(&use_count, ri) != 1 || at_u32(&inst_block, ri) != b || view.op(ri) != Op::Rotl || at_bool(&skip_emit, ri) {
                            return None;
                        }
                        let rat = to_idx(at_u32(view.data, ri));
                        let (src_v, sh_v) = (at_u32(view.extra, rat), to_idx(at_u32(view.extra, rat + 1)));
                        let is_one = view.op(sh_v) == Op::Splat && at_opt(&const_u64, to_idx(at_u32(view.data, sh_v))) == Some(1);
                        if is_one { Some((a_v, src_v, ri)) } else { None }
                    };
                    if let Some((a, mb, ri)) = try_rax1(rhs, lhs).or_else(|| try_rax1(lhs, rhs)) {
                        set_at(&mut fusion, inst, Fusion::Rax1 { a, b: mb });
                        set_at(&mut skip_emit, ri, true);
                        continue;
                    }
                }
                // 3. ARMv8.2-SHA3 3-way XOR: `xor(xor(a, b), c)` -> 1-instruction `eor3 vd.16b, va, vb, vc`!
                let try_xor = |x: u32, other: u32| -> Option<(u32, u32, u32)> {
                    let xi = to_idx(x);
                    if at_u32(&use_count, xi) == 1 && at_u32(&inst_block, xi) == b && view.op(xi) == Op::Xor && at_fusion(&fusion, xi) == Fusion::None && !at_bool(&skip_emit, xi) {
                        let xat = to_idx(at_u32(view.data, xi));
                        return Some((at_u32(view.extra, xat), at_u32(view.extra, xat + 1), other));
                    }
                    None
                };
                if let Some((a, mb, c)) = try_xor(lhs, rhs) {
                    set_at(&mut fusion, inst, Fusion::Eor3 { a, b: mb, c });
                    set_at(&mut skip_emit, to_idx(lhs), true);
                } else if let Some((a, mb, c)) = try_xor(rhs, lhs) {
                    set_at(&mut fusion, inst, Fusion::Eor3 { a, b: mb, c });
                    set_at(&mut skip_emit, to_idx(rhs), true);
                }
            }
            Op::Assume => {
                // Zero-cost compile-time semantic proof: emits 0 machine instructions!
                set_at(&mut skip_emit, inst, true);
            }
            Op::Br => {
                let at = to_idx(at_u32(view.data, inst));
                let cond_v = to_idx(at_u32(view.extra, at));
                let then_argc = to_idx(at_u32(view.extra, at + 2));
                let else_argc = to_idx(at_u32(view.extra, at + 3 + then_argc + 1));
                if at_u32(&use_count, cond_v) == 1 && at_u32(&inst_block, cond_v) == b && !at_bool(&skip_emit, cond_v) {
                    if view.op(cond_v) == Op::Project {
                        let pat = to_idx(at_u32(view.data, cond_v));
                        let call_v = to_idx(at_u32(view.extra, pat));
                        let field_idx = at_u32(view.extra, pat + 1);
                        if field_idx == 1
                            && at_u32(&inst_block, call_v) == b
                            && matches!(view.op(call_v), Op::Call | Op::CallIndirect)
                        {
                            let cat = to_idx(at_u32(view.data, call_v));
                            let sig_id = SigId(at_u32(view.extra, cat + 1));
                            let is_blaze_res = view
                                .sigs
                                .get(sig_id.index())
                                .is_some_and(|r| Conv::from_byte(r.conv) == Some(Conv::BlazeResult));
                            let flags_intact = is_blaze_res
                                && ((call_v + 1)..inst).all(|k| {
                                    at_bool(&skip_emit, k)
                                        || matches!(
                                            view.op(k),
                                            Op::Project | Op::Iconst | Op::IconstWide | Op::Assume
                                        )
                                });
                            if flags_intact {
                                set_at(&mut fusion, inst, Fusion::BlazeResultCallBr);
                                set_at(&mut skip_emit, cond_v, true);
                                set_at(&mut blaze_result_br_fused, call_v, true);
                            }
                        }
                    } else if view.op(cond_v) == Op::Icmp {
                        let cat = to_idx(at_u32(view.data, cond_v));
                        if let Some(cond) = IntCond::from_word(at_u32(view.extra, cat)) {
                            let a = at_u32(view.extra, cat + 1);
                            let bv = at_u32(view.extra, cat + 2);
                            let op_ty = ty_at(view.tys, to_idx(a));
                            if op_ty == Ty::I128 {
                                continue;
                            }
                            // Check `tbz`/`tbnz` and `tst` fusion for `icmp eq/ne (and(x, mask)), 0`!
                            let mut fused_bit_br = false;
                            if then_argc == 0
                                && else_argc == 0
                                && matches!(cond, IntCond::Eq | IntCond::Ne)
                                && at_opt(&const_u64, to_idx(bv)) == Some(0)
                                && op_ty.bits(64) == 64
                            {
                                let ai = to_idx(a);
                                if at_u32(&use_count, ai) == 1
                                    && at_u32(&inst_block, ai) == b
                                    && view.op(ai) == Op::And
                                    && at_fusion(&fusion, ai) == Fusion::None
                                    && !at_bool(&skip_emit, ai)
                                {
                                    let aat = to_idx(at_u32(view.data, ai));
                                    let x = at_u32(view.extra, aat);
                                    let y = at_u32(view.extra, aat + 1);
                                    let nonzero_is_then = cond == IntCond::Ne;
                                    let c_pair = if let Some(cv) = at_opt(&const_u64, to_idx(y)) {
                                        Some((x, y, cv))
                                    } else if let Some(cv) = at_opt(&const_u64, to_idx(x)) {
                                        Some((y, x, cv))
                                    } else {
                                        None
                                    };
                                    if let Some((val, mask_imm, cv)) = c_pair {
                                        if cv.is_power_of_two() && len < 2048 {
                                            set_at(&mut fusion, inst, Fusion::TbzBr {
                                                val,
                                                bit: cv.trailing_zeros(),
                                                nonzero_is_then,
                                                mask_imm,
                                            });
                                            set_at(&mut skip_emit, cond_v, true);
                                            set_at(&mut skip_emit, ai, true);
                                            fused_bit_br = true;
                                        } else if let Some(nrs) = enc::logical_imm(cv, 64) {
                                            set_at(&mut fusion, inst, Fusion::TstBr {
                                                a: val,
                                                rhs: mask_imm,
                                                imm_nrs: Some(u32_of(nrs)),
                                                nonzero_is_then,
                                            });
                                            set_at(&mut skip_emit, cond_v, true);
                                            set_at(&mut skip_emit, ai, true);
                                            fused_bit_br = true;
                                        }
                                    } else {
                                        set_at(&mut fusion, inst, Fusion::TstBr {
                                            a: x,
                                            rhs: y,
                                            imm_nrs: None,
                                            nonzero_is_then,
                                        });
                                        set_at(&mut skip_emit, cond_v, true);
                                        set_at(&mut skip_emit, ai, true);
                                        fused_bit_br = true;
                                    }
                                }
                            }
                            if !fused_bit_br {
                                set_at(&mut fusion, inst, Fusion::CmpBr { cond, a, b: bv, op_ty });
                                set_at(&mut skip_emit, cond_v, true);
                            }
                        }
                    } else if view.op(cond_v) == Op::Fcmp {
                        let cat = to_idx(at_u32(view.data, cond_v));
                        if let Some(fc) = FloatCond::from_word(at_u32(view.extra, cat)).and_then(fcond_of) {
                            let a = at_u32(view.extra, cat + 1);
                            let bv = at_u32(view.extra, cat + 2);
                            if let Some(double) = is_fp_ty(ty_at(view.tys, to_idx(a))) {
                                set_at(&mut fusion, inst, Fusion::FcmpBr { cond: fc, a, b: bv, double });
                                set_at(&mut skip_emit, cond_v, true);
                            }
                        }
                    }
                }
            }
            Op::Select if is_vec128_ty(ty).is_none() && ty != Ty::I128 => {
                let at = to_idx(at_u32(view.data, inst));
                let cond_v = to_idx(at_u32(view.extra, at));
                let t_val = at_u32(view.extra, at + 1);
                let f_val = at_u32(view.extra, at + 2);
                if at_u32(&use_count, cond_v) == 1 && at_u32(&inst_block, cond_v) == b && !at_bool(&skip_emit, cond_v) {
                    if view.op(cond_v) == Op::Icmp {
                        let cat = to_idx(at_u32(view.data, cond_v));
                        if let Some(cond) = IntCond::from_word(at_u32(view.extra, cat)) {
                            let a = at_u32(view.extra, cat + 1);
                            let bv = at_u32(view.extra, cat + 2);
                            let op_ty = ty_at(view.tys, to_idx(a));
                            if op_ty == Ty::I128 {
                                continue;
                            }
                            // Try branchless `csneg`/`csinv`/`csinc` for 64-bit integer select!
                            let mut special = false;
                            if bits == 64 && is_fp_ty(ty).is_none() {
                                let ti = to_idx(t_val);
                                let fi = to_idx(f_val);
                                // `select(c, x, neg(x))` -> `csneg rd, x, x, c` (branchless `abs`!)
                                if at_u32(&use_count, fi) == 1 && at_u32(&inst_block, fi) == b && view.op(fi) == Op::Neg && at_u32(view.data, fi) == t_val && !at_bool(&skip_emit, fi) {
                                    set_at(&mut fusion, inst, Fusion::CsnegSelect { cond, a, b: bv, op_ty, x: t_val, neg_when_false: true });
                                    set_at(&mut skip_emit, cond_v, true);
                                    set_at(&mut skip_emit, fi, true);
                                    special = true;
                                } else if at_u32(&use_count, ti) == 1 && at_u32(&inst_block, ti) == b && view.op(ti) == Op::Neg && at_u32(view.data, ti) == f_val && !at_bool(&skip_emit, ti) {
                                    set_at(&mut fusion, inst, Fusion::CsnegSelect { cond, a, b: bv, op_ty, x: f_val, neg_when_false: false });
                                    set_at(&mut skip_emit, cond_v, true);
                                    set_at(&mut skip_emit, ti, true);
                                    special = true;
                                } else if at_u32(&use_count, fi) == 1 && at_u32(&inst_block, fi) == b && view.op(fi) == Op::Not && at_u32(view.data, fi) == t_val && !at_bool(&skip_emit, fi) {
                                    set_at(&mut fusion, inst, Fusion::CsinvSelect { cond, a, b: bv, op_ty, x: t_val, inv_when_false: true });
                                    set_at(&mut skip_emit, cond_v, true);
                                    set_at(&mut skip_emit, fi, true);
                                    special = true;
                                } else if at_u32(&use_count, ti) == 1 && at_u32(&inst_block, ti) == b && view.op(ti) == Op::Not && at_u32(view.data, ti) == f_val && !at_bool(&skip_emit, ti) {
                                    set_at(&mut fusion, inst, Fusion::CsinvSelect { cond, a, b: bv, op_ty, x: f_val, inv_when_false: false });
                                    set_at(&mut skip_emit, cond_v, true);
                                    set_at(&mut skip_emit, ti, true);
                                    special = true;
                                } else if at_u32(&use_count, fi) == 1 && at_u32(&inst_block, fi) == b && view.op(fi) == Op::Add && at_fusion(&fusion, fi) == Fusion::None && !at_bool(&skip_emit, fi) {
                                    let fat = to_idx(at_u32(view.data, fi));
                                    let (fl, fr) = (at_u32(view.extra, fat), at_u32(view.extra, fat + 1));
                                    if (fl == t_val && at_opt(&const_u64, to_idx(fr)) == Some(1)) || (fr == t_val && at_opt(&const_u64, to_idx(fl)) == Some(1)) {
                                        set_at(&mut fusion, inst, Fusion::CsincSelect { cond, a, b: bv, op_ty, x: t_val, inc_when_false: true });
                                        set_at(&mut skip_emit, cond_v, true);
                                        set_at(&mut skip_emit, fi, true);
                                        special = true;
                                    }
                                }
                            }
                            if !special {
                                set_at(&mut fusion, inst, Fusion::CmpSelect { cond, a, b: bv, op_ty });
                                set_at(&mut skip_emit, cond_v, true);
                            }
                        }
                    } else if view.op(cond_v) == Op::Fcmp {
                        let cat = to_idx(at_u32(view.data, cond_v));
                        if let Some(fc) = FloatCond::from_word(at_u32(view.extra, cat)).and_then(fcond_of) {
                            let a = at_u32(view.extra, cat + 1);
                            let bv = at_u32(view.extra, cat + 2);
                            if let Some(double) = is_fp_ty(ty_at(view.tys, to_idx(a))) {
                                set_at(&mut fusion, inst, Fusion::FcmpSelect { cond: fc, a, b: bv, double });
                                set_at(&mut skip_emit, cond_v, true);
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
        if view.op(inst) == Op::SlotAddr && at_u32(&use_count, inst) > 0 && at_u32(&slot_addr_folded_uses, inst) == at_u32(&use_count, inst) {
            set_at(&mut skip_emit, inst, true);
        }
    }

    // 2b. Count immediate-operand uses over ONLY the surviving (non-skipped) instructions & fusions,
    //     so constants consumed solely in immediate positions need ZERO register & ZERO emission!
    let mut imm_uses: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    for _ in 0..len {
        imm_uses.push(0);
    }
    for inst in 0..len {
        if at_bool(&skip_emit, inst) {
            continue;
        }
        let op = view.op(inst);
        let ty = ty_at(view.tys, inst);
        let bits = if ty == Ty::NONE || is_fp_ty(ty).is_some() { 0 } else { ty.bits(64) };
        match at_fusion(&fusion, inst) {
            Fusion::AddLsl { imm_inst, .. }
            | Fusion::AddLsr { imm_inst, .. }
            | Fusion::SubLsl { imm_inst, .. }
            | Fusion::EorLsl { imm_inst, .. }
            | Fusion::EorLsr { imm_inst, .. }
            | Fusion::MemBaseOff { imm_inst, .. } => {
                inc_u32(&mut imm_uses, to_idx(imm_inst));
            }
            Fusion::MemScaledIdx { shift_imm, .. } => {
                inc_u32(&mut imm_uses, to_idx(shift_imm));
            }
            Fusion::TbzBr { mask_imm, .. } => {
                inc_u32(&mut imm_uses, to_idx(mask_imm));
            }
            Fusion::TstBr { rhs, imm_nrs: Some(_), .. } => {
                inc_u32(&mut imm_uses, to_idx(rhs));
            }
            Fusion::CmpBr { b: bv, op_ty, .. }
            | Fusion::CmpSelect { b: bv, op_ty, .. }
            | Fusion::CsnegSelect { b: bv, op_ty, .. }
            | Fusion::CsincSelect { b: bv, op_ty, .. }
            | Fusion::CsinvSelect { b: bv, op_ty, .. } => {
                if (op_ty.bits(64) == 64 || op_ty.bits(64) == 32)
                    && at_opt(&const_u64, to_idx(bv)).is_some_and(|v| v <= 4095)
                {
                    inc_u32(&mut imm_uses, to_idx(bv));
                }
            }
            Fusion::Madd { .. }
            | Fusion::Msub { .. }
            | Fusion::Fmadd { .. }
            | Fusion::Fmsub { .. }
            | Fusion::Eor3 { .. }
            | Fusion::Bcax { .. }
            | Fusion::Rax1 { .. }
            | Fusion::AdcsChain { .. }
            | Fusion::AdcsOverflowChain { .. }
            | Fusion::FcmpBr { .. }
            | Fusion::FcmpSelect { .. }
            | Fusion::MemSpOff { .. }
            | Fusion::MemRegIdx { .. }
            | Fusion::Bic { .. }
            | Fusion::Orn { .. }
            | Fusion::Eon { .. }
            | Fusion::TstBr { imm_nrs: None, .. }
            | Fusion::BlazeResultCallBr
            | Fusion::VecUadalp { .. } => {}
            Fusion::None => match op {
                Op::Add => {
                    let at = to_idx(at_u32(view.data, inst));
                    let lhs = to_idx(at_u32(view.extra, at));
                    let rhs = to_idx(at_u32(view.extra, at + 1));
                    if at_opt(&const_u64, rhs).is_some_and(|v| v <= 4095) {
                        inc_u32(&mut imm_uses, rhs);
                    } else if at_opt(&const_u64, lhs).is_some_and(|v| v <= 4095) {
                        inc_u32(&mut imm_uses, lhs);
                    }
                }
                Op::Sub => {
                    let at = to_idx(at_u32(view.data, inst));
                    let rhs = to_idx(at_u32(view.extra, at + 1));
                    if at_opt(&const_u64, rhs).is_some_and(|v| v <= 4095) {
                        inc_u32(&mut imm_uses, rhs);
                    }
                }
                Op::PtrAdd => {
                    let at = to_idx(at_u32(view.data, inst));
                    let rhs = to_idx(at_u32(view.extra, at + 1));
                    if let Some(cv) = at_opt(&const_u64, rhs) {
                        let by_bits = ty_at(view.tys, rhs).bits(64);
                        let delta = sign_extend_u64(cv, by_bits);
                        if (-4095..=4095).contains(&delta) {
                            inc_u32(&mut imm_uses, rhs);
                        }
                    }
                }
                Op::And | Op::Or | Op::Xor if bits == 64 || bits == 32 => {
                    let at = to_idx(at_u32(view.data, inst));
                    let lhs = to_idx(at_u32(view.extra, at));
                    let rhs = to_idx(at_u32(view.extra, at + 1));
                    let fits_log = |v: u64| {
                        enc::logical_imm(v, bits).is_some()
                            || (op == Op::And && bits == 64 && is_low_bitmask(v).is_some())
                    };
                    if at_opt(&const_u64, rhs).is_some_and(fits_log) {
                        inc_u32(&mut imm_uses, rhs);
                    } else if at_opt(&const_u64, lhs).is_some_and(fits_log) {
                        inc_u32(&mut imm_uses, lhs);
                    }
                }
                Op::And => {
                    let at = to_idx(at_u32(view.data, inst));
                    let lhs = to_idx(at_u32(view.extra, at));
                    let rhs = to_idx(at_u32(view.extra, at + 1));
                    if at_opt(&const_u64, rhs).and_then(is_low_bitmask).is_some() {
                        inc_u32(&mut imm_uses, rhs);
                    } else if at_opt(&const_u64, lhs).and_then(is_low_bitmask).is_some() {
                        inc_u32(&mut imm_uses, lhs);
                    }
                }
                Op::Shl | Op::Lshr | Op::Ashr | Op::Rotl | Op::Rotr if bits == 64 || bits == 32 => {
                    let at = to_idx(at_u32(view.data, inst));
                    let rhs = to_idx(at_u32(view.extra, at + 1));
                    if at_opt(&const_u64, rhs).is_some() {
                        inc_u32(&mut imm_uses, rhs);
                    }
                }
                Op::Udiv | Op::Urem if bits == 64 => {
                    let at = to_idx(at_u32(view.data, inst));
                    let rhs = to_idx(at_u32(view.extra, at + 1));
                    if at_opt(&const_u64, rhs).and_then(compute_magic_udiv64).is_some() {
                        inc_u32(&mut imm_uses, rhs);
                    }
                }
                Op::Memcpy | Op::Memset | Op::Memcmp => {
                    let at = to_idx(at_u32(view.data, inst));
                    let len_v = to_idx(at_u32(view.extra, at + 2));
                    if at_opt(&const_u64, len_v).is_some_and(|n| n <= SOTA_BULK_INLINE_MOST) {
                        inc_u32(&mut imm_uses, len_v);
                    }
                }
                Op::Return => {
                    let at = to_idx(at_u32(view.data, inst));
                    let retc = to_idx(at_u32(view.extra, at));
                    if retc == 2
                        && view.sigs.get(SigId(view.head.sig).index()).and_then(|r| Conv::from_byte(r.conv))
                            == Some(Conv::BlazeResult)
                    {
                        let err_v = to_idx(at_u32(view.extra, at + 2));
                        if at_opt(&const_u64, err_v).is_some() {
                            inc_u32(&mut imm_uses, err_v);
                        }
                    }
                }
                Op::Icmp => {
                    let at = to_idx(at_u32(view.data, inst));
                    let a = to_idx(at_u32(view.extra, at + 1));
                    let bv = to_idx(at_u32(view.extra, at + 2));
                    let abits = ty_at(view.tys, a).bits(64);
                    if (abits == 64 || abits == 32) && at_opt(&const_u64, bv).is_some_and(|v| v <= 4095) {
                        inc_u32(&mut imm_uses, bv);
                    }
                }
                _ => {}
            },
        }
    }
    for inst in 0..len {
        if at_opt(&const_u64, inst).is_some() && at_u32(&use_count, inst) > 0 && at_u32(&imm_uses, inst) == at_u32(&use_count, inst) {
            set_at(&mut skip_emit, inst, true);
        }
    }

    // 3. IPRA-aware Greedy Priority-Weighted Graph-Coloring Regalloc + True Constant Rematerialization!
    let reg_alloc = regalloc::allocate_with_ipra(
        &view,
        &skip_emit,
        &tail_calls,
        max_spill_slots,
        call_clobbers,
        alloc,
    )
    .map_err(|_| TierError::Refused {
        tier: TIER,
        inst: None,
        what: "spill slots exceeded Sota fast frame budget",
    })?;

    let used_bytes = u64::from(spill_base) + u64::from(reg_alloc.num_spill_slots) * 8;
    let frame_size_u64 = used_bytes.next_multiple_of(16);
    if frame_size_u64 + (most_align - 16) >= FRAME_LIMIT {
        return Err(TierError::Refused { tier: TIER, inst: None, what: "stack frame >= 16 MiB" });
    }
    let frame_size = u32_of(frame_size_u64);
    let internal_err: core::cell::Cell<Option<TierError>> = core::cell::Cell::new(None);

    // Helper closures for register / spill / remat access.
    let load_gpr = |words: &mut Vec<u32, Alloc<'a>>, v: u32, scratch: Reg| -> Reg {
        match at_loc(&reg_alloc.loc_of, to_idx(v)) {
            AllocLoc::Gpr(r) => u32::from(r),
            AllocLoc::GprSpill(slot) => {
                emit_ld_spill_gpr(words, scratch, spill_base, slot);
                scratch
            }
            AllocLoc::RematConst(cval) => {
                emit_const_u64(words, scratch, cval);
                scratch
            }
            AllocLoc::None => {
                if let Some(cval) = at_opt(&const_u64, to_idx(v)) {
                    emit_const_u64(words, scratch, cval);
                    scratch
                } else {
                    internal_err.set(Some(TierError::Refused { tier: TIER, inst: Some(v), what: "unallocated GPR value" }));
                    scratch
                }
            }
            _ => {
                internal_err.set(Some(TierError::Refused { tier: TIER, inst: Some(v), what: "expected GPR value" }));
                scratch
            }
        }
    };

    let load_fp = |words: &mut Vec<u32, Alloc<'a>>, v: u32, scratch: Reg| -> Reg {
        match at_loc(&reg_alloc.loc_of, to_idx(v)) {
            AllocLoc::Fp(r) => u32::from(r),
            AllocLoc::FpSpill(slot) => {
                emit_ld_spill_fp(words, scratch, spill_base, slot);
                scratch
            }
            _ => {
                internal_err.set(Some(TierError::Refused { tier: TIER, inst: Some(v), what: "expected FP value" }));
                scratch
            }
        }
    };

    let dst_gpr = |inst: usize| -> (Reg, Option<u32>) {
        match at_loc(&reg_alloc.loc_of, inst) {
            AllocLoc::Gpr(r) => (u32::from(r), None),
            AllocLoc::GprSpill(slot) => (X16, Some(slot)),
            AllocLoc::None => (X16, None),
            _ => {
                internal_err.set(Some(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "expected GPR dest" }));
                (X16, None)
            }
        }
    };

    let dst_fp = |inst: usize| -> (Reg, Option<u32>) {
        match at_loc(&reg_alloc.loc_of, inst) {
            AllocLoc::Fp(r) => (u32::from(r), None),
            AllocLoc::FpSpill(slot) => (V30, Some(slot)),
            AllocLoc::None => (V30, None),
            _ => {
                internal_err.set(Some(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "expected FP dest" }));
                (V30, None)
            }
        }
    };

    let load_vec = |words: &mut Vec<u32, Alloc<'a>>, v: u32, scratch: Reg| -> Reg {
        match at_loc(&reg_alloc.loc_of, to_idx(v)) {
            AllocLoc::Vec(r) => u32::from(r),
            AllocLoc::VecSpill(slot) => {
                emit_ld_spill_vec(words, scratch, spill_base, slot);
                scratch
            }
            _ => {
                internal_err.set(Some(TierError::Refused { tier: TIER, inst: Some(v), what: "expected Vec128 value" }));
                scratch
            }
        }
    };

    let dst_vec = |inst: usize| -> (Reg, Option<u32>) {
        match at_loc(&reg_alloc.loc_of, inst) {
            AllocLoc::Vec(r) => (u32::from(r), None),
            AllocLoc::VecSpill(slot) => (V30, Some(slot)),
            AllocLoc::None => (V30, None),
            _ => {
                internal_err.set(Some(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "expected Vec128 dest" }));
                (V30, None)
            }
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

    // Precompute cleanup blocks (reachable from `Cleanup`/`Terminate` landing blocks via ordinary edges)
    // and collect `(call_byte_off, Option<unwind_block>)` so unwind rows resolve once all blocks are placed.
    let mut is_cleanup_block: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
    let mut cleanup_todo: Vec<usize, Alloc<'a>> = Vec::new_in(alloc);
    let mut has_unwinding = false;
    for b in 0..blocks {
        let kind = view.block_kind(Block(u32_of(b)));
        if kind.is_landing() {
            has_unwinding = true;
        }
        let is_c = matches!(kind, BlockKind::Cleanup | BlockKind::Terminate);
        is_cleanup_block.push(is_c);
        if is_c {
            cleanup_todo.push(b);
        }
    }
    if !has_unwinding {
        has_unwinding = (0..len).any(|i| matches!(view.op(i), Op::Invoke | Op::InvokeIndirect | Op::Resume));
    }
    while let Some(cb) = cleanup_todo.pop() {
        let term = view.terminator(Block(u32_of(cb)));
        let top = view.op(term);
        if matches!(top, Op::Invoke | Op::InvokeIndirect) {
            // Only the ordinary normal-return edge propagates cleanup status (not the unwind edge).
            let normal = to_idx(at_u32(view.extra, to_idx(at_u32(view.data, term)) + 2));
            if !at_bool(&is_cleanup_block, normal) {
                set_at(&mut is_cleanup_block, normal, true);
                cleanup_todo.push(normal);
            }
        } else {
            regalloc::for_each_edge(&view, term, |target, _, _| {
                let tb = to_idx(target);
                if !at_bool(&is_cleanup_block, tb) {
                    set_at(&mut is_cleanup_block, tb, true);
                    cleanup_todo.push(tb);
                }
            });
        }
    }
    let mut pending_unwind: Vec<(u32, Option<u32>), Alloc<'a>> = Vec::new_in(alloc);

    // Frameless leaf prologue when no calls, callee-saved regs, stack slots, spills, or unwinding!
    let x_pairs = reg_alloc.x_pairs;
    let d_pairs = reg_alloc.d_pairs;
    let cs_bytes = (x_pairs.count_ones() + d_pairs.count_ones()) * 16;
    let needs_frame = reg_alloc.has_calls
        || x_pairs != 0
        || d_pairs != 0
        || frame_size > 0
        || most_align > 16
        || has_stacked_in
        || has_unwinding;

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
                    let delta = word_delta(probe_loop, words.len());
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

    // Marshal entry parameters from `X0..X7` (`X8` for `sret`), `V0..V7`, and caller stack `[FP, #(16 + off)]`.
    let func_sig = view.sig(SigId(view.head.sig));
    let entry_params = view.params(Block(0));
    gpr_moves.clear();
    fp_moves.clear();
    let mut stacked_in: Vec<(usize, Ty, Stacked), Alloc<'a>> = Vec::new_in(alloc);
    let mut gpr_arg_idx = 0usize;
    let mut fp_arg_idx = 0usize;
    let mut entry_nsaa = Nsaa::new(os);
    for (p, param) in entry_params.clone().zip(func_sig.params.iter().copied()) {
        let ty = ty_at(view.tys, p);
        let Some(placed) = place_param(&param, &mut gpr_arg_idx, &mut fp_arg_idx, &mut entry_nsaa) else {
            return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(p)), what: "unplaceable entry param" });
        };
        match placed {
            ParamPlace::Sret => match at_loc(&reg_alloc.loc_of, p) {
                AllocLoc::Gpr(dst) => gpr_moves.push((GprCopyLoc::Reg(u32::from(dst)), GprCopyLoc::Reg(X8))),
                AllocLoc::GprSpill(slot) => emit_st_spill_gpr(&mut words, X8, spill_base, slot),
                AllocLoc::None => {}
                _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(p)), what: "unexpected sret entry param loc" }),
            },
            ParamPlace::Fp(src) => match at_loc(&reg_alloc.loc_of, p) {
                AllocLoc::Fp(dst) => fp_moves.push((FpCopyLoc::Reg(u32::from(dst)), FpCopyLoc::Reg(src))),
                AllocLoc::FpSpill(slot) => emit_st_spill_fp(&mut words, src, spill_base, slot),
                AllocLoc::None => {}
                _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(p)), what: "unexpected FP entry param loc" }),
            },
            ParamPlace::GprPair(src_lo, src_hi) => match at_loc(&reg_alloc.loc_of, p) {
                AllocLoc::GprPair(d0, d1) => {
                    gpr_moves.push((GprCopyLoc::Reg(u32::from(d0)), GprCopyLoc::Reg(src_lo)));
                    gpr_moves.push((GprCopyLoc::Reg(u32::from(d1)), GprCopyLoc::Reg(src_hi)));
                }
                AllocLoc::GprPairSpill(s0, s1) => {
                    emit_st_spill_gpr(&mut words, src_lo, spill_base, s0);
                    emit_st_spill_gpr(&mut words, src_hi, spill_base, s1);
                }
                AllocLoc::None => {}
                _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(p)), what: "unexpected i128 entry param loc" }),
            },
            ParamPlace::Gpr(src) => {
                let bits = ty.bits(64);
                match at_loc(&reg_alloc.loc_of, p) {
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
                    _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(p)), what: "unexpected GPR entry param loc" }),
                }
            }
            ParamPlace::Stack(slot) => {
                stacked_in.push((p, ty, slot));
            }
        }
    }
    emit_parallel_gpr_moves(&mut words, &mut gpr_moves, spill_base)?;
    emit_parallel_fp_moves(&mut words, &mut fp_moves, spill_base)?;
    // Load stacked entry parameters from `[FP, #(16 + slot.at)]` after register moves so nothing is clobbered.
    for &(p, ty, slot) in stacked_in.iter() {
        let off = 16 + slot.at;
        if is_fp_ty(ty).is_some() {
            let sz = size_of_stack_bytes(slot.bytes);
            match at_loc(&reg_alloc.loc_of, p) {
                AllocLoc::Fp(dst) => emit_mem_off(&mut words, true, true, sz, u32::from(dst), FP, off),
                AllocLoc::FpSpill(s) => {
                    emit_mem_off(&mut words, true, true, sz, V30, FP, off);
                    emit_st_spill_fp(&mut words, V30, spill_base, s);
                }
                AllocLoc::None => {}
                _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(p)), what: "unexpected stacked FP entry param loc" }),
            }
        } else if ty == Ty::I128 {
            match at_loc(&reg_alloc.loc_of, p) {
                AllocLoc::GprPair(d0, d1) => {
                    emit_mem_off(&mut words, true, false, Size::X, u32::from(d0), FP, off);
                    emit_mem_off(&mut words, true, false, Size::X, u32::from(d1), FP, off + 8);
                }
                AllocLoc::GprPairSpill(s0, s1) => {
                    emit_mem_off(&mut words, true, false, Size::X, X16, FP, off);
                    emit_st_spill_gpr(&mut words, X16, spill_base, s0);
                    emit_mem_off(&mut words, true, false, Size::X, X16, FP, off + 8);
                    emit_st_spill_gpr(&mut words, X16, spill_base, s1);
                }
                AllocLoc::None => {}
                _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(p)), what: "unexpected stacked i128 entry param loc" }),
            }
        } else {
            let sz = size_of_stack_bytes(slot.bytes);
            let bits = ty.bits(64);
            match at_loc(&reg_alloc.loc_of, p) {
                AllocLoc::Gpr(dst) => {
                    emit_mem_off(&mut words, true, false, sz, u32::from(dst), FP, off);
                }
                AllocLoc::GprSpill(s) => {
                    emit_mem_off(&mut words, true, false, sz, X16, FP, off);
                    if bits < 64 {
                        words.push(enc::zext(X16, X16, bits));
                    }
                    emit_st_spill_gpr(&mut words, X16, spill_base, s);
                }
                AllocLoc::None => {}
                _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(p)), what: "unexpected stacked GPR entry param loc" }),
            }
        }
    }
    // Narrow sub-64-bit register-resident GPR entry params to preserve zero-extension invariant.
    for p in entry_params {
        if let AllocLoc::Gpr(dst) = at_loc(&reg_alloc.loc_of, p) {
            let ty = ty_at(view.tys, p);
            if is_fp_ty(ty).is_none() {
                emit_narrow(&mut words, u32::from(dst), ty.bits(64));
            }
        }
    }

    // Helper: emit signed/unsigned integer compare of `a` and `b` setting NZCV flags (native 32-bit W for i32!).
    let emit_cmp = |words: &mut Vec<u32, Alloc<'a>>, cond: IntCond, a: u32, bv: u32, op_ty: Ty| {
        let bits = op_ty.bits(64);
        let ra = load_gpr(words, a, X16);
        let signed = matches!(cond, IntCond::Slt | IntCond::Sle | IntCond::Sgt | IntCond::Sge);
        if let Some(imm) = at_opt(&const_u64, to_idx(bv)).filter(|&v| bits == 64 && v <= 4095) {
            words.push(enc::cmp_imm(ra, trunc_u32(imm)));
        } else if let Some(imm) = at_opt(&const_u64, to_idx(bv)).filter(|&v| bits == 32 && v <= 4095) {
            words.push(enc::sf(false, enc::cmp_imm(ra, trunc_u32(imm))));
        } else if bits == 32 {
            let rb = load_gpr(words, bv, X17);
            words.push(enc::sf(false, enc::cmp(ra, rb)));
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
            let dst_loc = at_loc(&reg_alloc.loc_of, param_inst);
            if dst_loc == AllocLoc::None {
                continue;
            }
            let src_v = at_u32(view.extra, args_start + k);
            match dst_loc {
                AllocLoc::Gpr(d) => {
                    let dst = GprCopyLoc::Reg(u32::from(d));
                    match at_loc(&reg_alloc.loc_of, to_idx(src_v)) {
                        AllocLoc::Gpr(s) => gpr_moves.push((dst, GprCopyLoc::Reg(u32::from(s)))),
                        AllocLoc::GprSpill(s) => gpr_moves.push((dst, GprCopyLoc::Spill(s))),
                        AllocLoc::RematConst(cval) => const_writes.push((dst, cval)),
                        AllocLoc::None => {
                            if let Some(cval) = at_opt(&const_u64, to_idx(src_v)) {
                                const_writes.push((dst, cval));
                            } else {
                                internal_err.set(Some(TierError::Refused { tier: TIER, inst: Some(src_v), what: "unallocated non-const edge arg" }));
                            }
                        }
                        _ => internal_err.set(Some(TierError::Refused { tier: TIER, inst: Some(src_v), what: "unexpected GPR edge arg loc" })),
                    }
                }
                AllocLoc::GprSpill(d) => {
                    let dst = GprCopyLoc::Spill(d);
                    match at_loc(&reg_alloc.loc_of, to_idx(src_v)) {
                        AllocLoc::Gpr(s) => gpr_moves.push((dst, GprCopyLoc::Reg(u32::from(s)))),
                        AllocLoc::GprSpill(s) => gpr_moves.push((dst, GprCopyLoc::Spill(s))),
                        AllocLoc::RematConst(cval) => const_writes.push((dst, cval)),
                        AllocLoc::None => {
                            if let Some(cval) = at_opt(&const_u64, to_idx(src_v)) {
                                const_writes.push((dst, cval));
                            } else {
                                internal_err.set(Some(TierError::Refused { tier: TIER, inst: Some(src_v), what: "unallocated non-const spill edge arg" }));
                            }
                        }
                        _ => internal_err.set(Some(TierError::Refused { tier: TIER, inst: Some(src_v), what: "unexpected GPR spill edge arg loc" })),
                    }
                }
                AllocLoc::Fp(d) => {
                    let dst = FpCopyLoc::Reg(u32::from(d));
                    match at_loc(&reg_alloc.loc_of, to_idx(src_v)) {
                        AllocLoc::Fp(s) => fp_moves.push((dst, FpCopyLoc::Reg(u32::from(s)))),
                        AllocLoc::FpSpill(s) => fp_moves.push((dst, FpCopyLoc::Spill(s))),
                        _ => internal_err.set(Some(TierError::Refused { tier: TIER, inst: Some(src_v), what: "unexpected FP edge arg loc" })),
                    }
                }
                AllocLoc::FpSpill(d) => {
                    let dst = FpCopyLoc::Spill(d);
                    match at_loc(&reg_alloc.loc_of, to_idx(src_v)) {
                        AllocLoc::Fp(s) => fp_moves.push((dst, FpCopyLoc::Reg(u32::from(s)))),
                        AllocLoc::FpSpill(s) => fp_moves.push((dst, FpCopyLoc::Spill(s))),
                        _ => internal_err.set(Some(TierError::Refused { tier: TIER, inst: Some(src_v), what: "unexpected FP spill edge arg loc" })),
                    }
                }
                AllocLoc::Vec(d) => {
                    let dst = FpCopyLoc::Reg(u32::from(d));
                    match at_loc(&reg_alloc.loc_of, to_idx(src_v)) {
                        AllocLoc::Vec(s) => fp_moves.push((dst, FpCopyLoc::Reg(u32::from(s)))),
                        AllocLoc::VecSpill(s) => fp_moves.push((dst, FpCopyLoc::VecSpill(s))),
                        _ => internal_err.set(Some(TierError::Refused { tier: TIER, inst: Some(src_v), what: "unexpected Vec128 edge arg loc" })),
                    }
                }
                AllocLoc::VecSpill(d) => {
                    let dst = FpCopyLoc::VecSpill(d);
                    match at_loc(&reg_alloc.loc_of, to_idx(src_v)) {
                        AllocLoc::Vec(s) => fp_moves.push((dst, FpCopyLoc::Reg(u32::from(s)))),
                        AllocLoc::VecSpill(s) => fp_moves.push((dst, FpCopyLoc::VecSpill(s))),
                        _ => internal_err.set(Some(TierError::Refused { tier: TIER, inst: Some(src_v), what: "unexpected Vec128 spill edge arg loc" })),
                    }
                }
                AllocLoc::GprPair(d0, d1) => {
                    let dst0 = GprCopyLoc::Reg(u32::from(d0));
                    let dst1 = GprCopyLoc::Reg(u32::from(d1));
                    match at_loc(&reg_alloc.loc_of, to_idx(src_v)) {
                        AllocLoc::GprPair(s0, s1) => {
                            gpr_moves.push((dst0, GprCopyLoc::Reg(u32::from(s0))));
                            gpr_moves.push((dst1, GprCopyLoc::Reg(u32::from(s1))));
                        }
                        AllocLoc::GprPairSpill(s0, s1) => {
                            gpr_moves.push((dst0, GprCopyLoc::Spill(s0)));
                            gpr_moves.push((dst1, GprCopyLoc::Spill(s1)));
                        }
                        _ => internal_err.set(Some(TierError::Refused { tier: TIER, inst: Some(src_v), what: "unexpected GprPair edge arg loc" })),
                    }
                }
                AllocLoc::GprPairSpill(d0, d1) => {
                    let dst0 = GprCopyLoc::Spill(d0);
                    let dst1 = GprCopyLoc::Spill(d1);
                    match at_loc(&reg_alloc.loc_of, to_idx(src_v)) {
                        AllocLoc::GprPair(s0, s1) => {
                            gpr_moves.push((dst0, GprCopyLoc::Reg(u32::from(s0))));
                            gpr_moves.push((dst1, GprCopyLoc::Reg(u32::from(s1))));
                        }
                        AllocLoc::GprPairSpill(s0, s1) => {
                            gpr_moves.push((dst0, GprCopyLoc::Spill(s0)));
                            gpr_moves.push((dst1, GprCopyLoc::Spill(s1)));
                        }
                        _ => internal_err.set(Some(TierError::Refused { tier: TIER, inst: Some(src_v), what: "unexpected GprPairSpill edge arg loc" })),
                    }
                }
                AllocLoc::None | AllocLoc::RematConst(_) => {}
            }
        }
        if let Err(err) = emit_parallel_moves(words, gpr_moves, spill_base) {
            internal_err.set(Some(err));
            return;
        }
        if let Err(err) = emit_parallel_moves(words, fp_moves, spill_base) {
            internal_err.set(Some(err));
            return;
        }
        // Materialize skipped constant edge arguments AFTER parallel moves so they never clobber any source!
        for &(dst, cval) in const_writes.iter() {
            match dst {
                GprCopyLoc::Reg(r) => emit_const_u64(words, r, cval),
                GprCopyLoc::Spill(s) => {
                    emit_const_u64(words, X16, cval);
                    emit_st_spill_gpr(words, X16, spill_base, s);
                }
                GprCopyLoc::VecSpill(_) => {
                    internal_err.set(Some(TierError::Refused { tier: TIER, inst: None, what: "GPR const edge cannot target VecSpill" }));
                }
            }
        }
    };

    // Identify loop-header blocks (targets of backward edges) for 16-byte fetch-boundary alignment.
    let mut is_loop_header: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        is_loop_header.push(false);
    }
    for b in 0..blocks {
        let term = view.terminator(Block(u32_of(b)));
        regalloc::for_each_edge(&view, term, |target, _, _| {
            if to_idx(target) <= b {
                set_at(&mut is_loop_header, to_idx(target), true);
            }
        });
    }

    // Helper closure: extract `(base_reg_or_sp, byte_off, is_fp)` for a 64-bit non-volatile
    // GPR or F64 load/store eligible for adjacent `ldp`/`stp` pair fusion.
    let mem64_pair_addr = |inst: usize, is_load: bool| -> Option<(Reg, i64, bool)> {
        if Flags(at_u8(view.flags, inst)).contains(Flags::VOLATILE) {
            return None;
        }
        let op = view.op(inst);
        if (is_load && op != Op::Load) || (!is_load && op != Op::Store) {
            return None;
        }
        let access_ty = if is_load {
            ty_at(view.tys, inst)
        } else {
            let at = to_idx(at_u32(view.data, inst));
            ty_at(view.tys, to_idx(at_u32(view.extra, at)))
        };
        let (size, is_fp) = mem_size_of(access_ty)?;
        if size != Size::X {
            return None;
        }
        match at_fusion(&fusion, inst) {
            Fusion::MemSpOff { off } => Some((SP, off, is_fp)),
            Fusion::MemBaseOff { base, off, .. } => match at_loc(&reg_alloc.loc_of, to_idx(base)) {
                AllocLoc::Gpr(r) => Some((u32::from(r), off, is_fp)),
                _ => None,
            },
            Fusion::None => {
                let at = to_idx(at_u32(view.data, inst));
                let (ptr, off) = if is_load {
                    (at_u32(view.extra, at), i64::from(at_u32(view.extra, at + 1).cast_signed()))
                } else {
                    (at_u32(view.extra, at + 1), i64::from(at_u32(view.extra, at + 2).cast_signed()))
                };
                match at_loc(&reg_alloc.loc_of, to_idx(ptr)) {
                    AllocLoc::Gpr(r) => Some((u32::from(r), off, is_fp)),
                    _ => None,
                }
            }
            _ => None,
        }
    };

    let mem128_pair_addr = |inst: usize, is_load: bool| -> Option<(Reg, i64)> {
        if Flags(at_u8(view.flags, inst)).contains(Flags::VOLATILE) {
            return None;
        }
        let op = view.op(inst);
        if (is_load && op != Op::Load) || (!is_load && op != Op::Store) {
            return None;
        }
        let access_ty = if is_load {
            ty_at(view.tys, inst)
        } else {
            let at = to_idx(at_u32(view.data, inst));
            ty_at(view.tys, to_idx(at_u32(view.extra, at)))
        };
        is_vec128_ty(access_ty)?;
        match at_fusion(&fusion, inst) {
            Fusion::MemSpOff { off } => Some((SP, off)),
            Fusion::MemBaseOff { base, off, .. } => match at_loc(&reg_alloc.loc_of, to_idx(base)) {
                AllocLoc::Gpr(r) => Some((u32::from(r), off)),
                _ => None,
            },
            Fusion::None => {
                let at = to_idx(at_u32(view.data, inst));
                let (ptr, off) = if is_load {
                    (at_u32(view.extra, at), i64::from(at_u32(view.extra, at + 1).cast_signed()))
                } else {
                    (at_u32(view.extra, at + 1), i64::from(at_u32(view.extra, at + 2).cast_signed()))
                };
                match at_loc(&reg_alloc.loc_of, to_idx(ptr)) {
                    AllocLoc::Gpr(r) => Some((u32::from(r), off)),
                    _ => None,
                }
            }
            _ => None,
        }
    };

    // Helper: emit function epilogue (shared by `Op::Return` and Tail-Call Optimization).
    let emit_epilogue = |words: &mut Vec<u32, Alloc<'a>>| {
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
    };

    // Precompute instruction-order ordinals of `Op::Unreachable` so `out.traps` preserves exact
    // `TrapSource::ordinal` mapping even when `compute_block_order` reorders blocks!
    let mut unreach_ord: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    let mut next_uord = 0u32;
    for i in 0..len {
        if view.op(i) == Op::Unreachable {
            unreach_ord.push(next_uord);
            next_uord += 1;
        } else {
            unreach_ord.push(u32::MAX);
        }
    }

    // Emit each block in hot-first order (cold/trap blocks sunk to tail; loop headers 16B-aligned)!
    for (order_idx, &b) in block_order.iter().enumerate() {
        let next_block = block_order.get(order_idx + 1).copied();
        if b != 0 && at_bool(&is_loop_header, b) {
            while words.len() % 4 != 0 {
                words.push(enc::nop());
            }
        }
        set_at(&mut block_word_off, b, words.len());
        if view.block_kind(Block(u32_of(b))).is_landing() {
            // The unwinder enters a landing block with the exception pointer in `X0` and `SP`/`FP`
            // restored (`Sota` never shifts `SP` around calls, so no `restore_sp` is needed).
            let exc_param = view.params(Block(u32_of(b))).start;
            match at_loc(&reg_alloc.loc_of, exc_param) {
                AllocLoc::Gpr(dst) => {
                    let rd = u32::from(dst);
                    if rd != X0 {
                        words.push(enc::mov(rd, X0));
                    }
                }
                AllocLoc::GprSpill(s) => emit_st_spill_gpr(&mut words, X0, spill_base, s),
                AllocLoc::None => {}
                _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(exc_param)), what: "unexpected landing block param loc" }),
            }
        }
        let range = view.block(Block(u32_of(b)));
        let mut inst = range.start;
        while inst < range.end {
            if at_bool(&skip_emit, inst) {
                inst += 1;
                continue;
            }
            // Adjacent 64-bit `ldp`/`stp` pair fusion: fuse `[base, #off]` + `[base, #off+8]`!
            if inst + 1 < range.end && !at_bool(&skip_emit, inst + 1) {
                if let (Some((base1, off1, fp1)), Some((base2, off2, fp2))) =
                    (mem64_pair_addr(inst, true), mem64_pair_addr(inst + 1, true))
                {
                    if fp1 == fp2
                        && base1 == base2
                        && off2 == off1 + 8
                        && off1 % 8 == 0
                        && (-512..=504).contains(&off1)
                    {
                        if !fp1 {
                            if let (AllocLoc::Gpr(r1), AllocLoc::Gpr(r2)) =
                                (at_loc(&reg_alloc.loc_of, inst), at_loc(&reg_alloc.loc_of, inst + 1))
                            {
                                let (rt1, rt2) = (u32::from(r1), u32::from(r2));
                                if rt1 != rt2 && rt1 != base1 {
                                    words.push(enc::ldp_off(rt1, rt2, base1, i32_of(off1)));
                                    inst += 2;
                                    continue;
                                }
                            }
                        } else if let (AllocLoc::Fp(r1), AllocLoc::Fp(r2)) =
                            (at_loc(&reg_alloc.loc_of, inst), at_loc(&reg_alloc.loc_of, inst + 1))
                        {
                            let (dt1, dt2) = (u32::from(r1), u32::from(r2));
                            if dt1 != dt2 {
                                words.push(enc::ldp_d_off(dt1, dt2, base1, i32_of(off1)));
                                inst += 2;
                                continue;
                            }
                        }
                    }
                } else if let (Some((base1, off1, fp1)), Some((base2, off2, fp2))) =
                    (mem64_pair_addr(inst, false), mem64_pair_addr(inst + 1, false))
                {
                    if fp1 == fp2
                        && base1 == base2
                        && off2 == off1 + 8
                        && off1 % 8 == 0
                        && (-512..=504).contains(&off1)
                    {
                        let v1 = at_u32(view.extra, to_idx(at_u32(view.data, inst)));
                        let v2 = at_u32(view.extra, to_idx(at_u32(view.data, inst + 1)));
                        if !fp1 {
                            if let (AllocLoc::Gpr(r1), AllocLoc::Gpr(r2)) =
                                (at_loc(&reg_alloc.loc_of, to_idx(v1)), at_loc(&reg_alloc.loc_of, to_idx(v2)))
                            {
                                words.push(enc::stp_off(u32::from(r1), u32::from(r2), base1, i32_of(off1)));
                                inst += 2;
                                continue;
                            }
                        } else if let (AllocLoc::Fp(r1), AllocLoc::Fp(r2)) =
                            (at_loc(&reg_alloc.loc_of, to_idx(v1)), at_loc(&reg_alloc.loc_of, to_idx(v2)))
                        {
                            words.push(enc::stp_d_off(u32::from(r1), u32::from(r2), base1, i32_of(off1)));
                            inst += 2;
                            continue;
                        }
                    }
                } else if let (Some((base1, off1)), Some((base2, off2))) =
                    (mem128_pair_addr(inst, true), mem128_pair_addr(inst + 1, true))
                {
                    // Adjacent 128-bit NEON vector pair load -> `ldp q1, q2, [base, #off]` (32 bytes / 256 bits in 1 op)!
                    if base1 == base2 && off2 == off1 + 16 && off1 % 16 == 0 && (-1024..=1008).contains(&off1) {
                        if let (AllocLoc::Vec(v1), AllocLoc::Vec(v2)) =
                            (at_loc(&reg_alloc.loc_of, inst), at_loc(&reg_alloc.loc_of, inst + 1))
                        {
                            if v1 != v2 {
                                words.push(enc::ldp_q_off(u32::from(v1), u32::from(v2), base1, i32_of(off1 / 16)));
                                inst += 2;
                                continue;
                            }
                        }
                    }
                } else if let (Some((base1, off1)), Some((base2, off2))) =
                    (mem128_pair_addr(inst, false), mem128_pair_addr(inst + 1, false))
                {
                    // Adjacent 128-bit NEON vector pair store -> `stp q1, q2, [base, #off]` (32 bytes in 1 op)!
                    if base1 == base2 && off2 == off1 + 16 && off1 % 16 == 0 && (-1024..=1008).contains(&off1) {
                        let v1 = at_u32(view.extra, to_idx(at_u32(view.data, inst)));
                        let v2 = at_u32(view.extra, to_idx(at_u32(view.data, inst + 1)));
                        if let (AllocLoc::Vec(r1), AllocLoc::Vec(r2)) =
                            (at_loc(&reg_alloc.loc_of, to_idx(v1)), at_loc(&reg_alloc.loc_of, to_idx(v2)))
                        {
                            words.push(enc::stp_q_off(u32::from(r1), u32::from(r2), base1, i32_of(off1 / 16)));
                            inst += 2;
                            continue;
                        }
                    }
                }
            }

            let op = view.op(inst);
            let ty = ty_at(view.tys, inst);
            let fp_ty = is_fp_ty(ty);
            // 128-bit NEON vector lane-mask comparison (`Icmp` -> `<lanes x i1>` all-ones/all-zeros mask).
            if is_vec_mask_ty(ty).is_some() && op == Op::Icmp {
                let at = to_idx(at_u32(view.data, inst));
                let cond = IntCond::from_word(at_u32(view.extra, at)).ok_or(TierError::Refused {
                    tier: TIER,
                    inst: Some(u32_of(inst)),
                    what: "invalid vector icmp cond",
                })?;
                let a_v = at_u32(view.extra, at + 1);
                let b_v = at_u32(view.extra, at + 2);
                let (vsize, _) = is_vec128_ty(ty_at(view.tys, to_idx(a_v))).ok_or(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "non-vec128 type" })?;
                let (vd, sp) = dst_vec(inst);
                let va = load_vec(&mut words, a_v, V30);
                let vb = load_vec(&mut words, b_v, V31);
                match cond {
                    IntCond::Eq => words.push(enc::cmeq_v(true, vsize, vd, va, vb)),
                    IntCond::Ne => {
                        words.push(enc::cmeq_v(true, vsize, vd, va, vb));
                        words.push(enc::not_v(vd, vd));
                    }
                    IntCond::Sgt => words.push(enc::cmgt_v(false, vsize, vd, va, vb)),
                    IntCond::Sge => words.push(enc::cmge_v(false, vsize, vd, va, vb)),
                    IntCond::Slt => words.push(enc::cmgt_v(false, vsize, vd, vb, va)),
                    IntCond::Sle => words.push(enc::cmge_v(false, vsize, vd, vb, va)),
                    IntCond::Ugt => words.push(enc::cmgt_v(true, vsize, vd, va, vb)),
                    IntCond::Uge => words.push(enc::cmge_v(true, vsize, vd, va, vb)),
                    IntCond::Ult => words.push(enc::cmgt_v(true, vsize, vd, vb, va)),
                    IntCond::Ule => words.push(enc::cmge_v(true, vsize, vd, vb, va)),
                }
                if let Some(s) = sp {
                    emit_st_spill_vec(&mut words, vd, spill_base, s);
                }
                inst += 1;
                continue;
            }
            let vec_ty = is_vec128_ty(ty);
            let bits = if ty == Ty::NONE || fp_ty.is_some() || vec_ty.is_some() || ty.compound_row().is_some() {
                0
            } else {
                ty.bits(64)
            };
            let loc = at_loc(&reg_alloc.loc_of, inst);

            // Skip dead pure non-side-effecting instructions (never skip terminators, calls, mem, atomics, traps, div/rem).
            let has_side_effect = op.is_terminator()
                || matches!(
                    op,
                    Op::Call
                        | Op::CallIndirect
                        | Op::Load
                        | Op::Store
                        | Op::Memcpy
                        | Op::Memmove
                        | Op::Memset
                        | Op::Memcmp
                        | Op::TargetIntrinsic
                        | Op::TlsAddr
                        | Op::AtomicLoad
                        | Op::AtomicStore
                        | Op::AtomicRmw
                        | Op::Cmpxchg
                        | Op::Fence
                        | Op::Trap
                        | Op::Udiv
                        | Op::Sdiv
                        | Op::Urem
                        | Op::Srem
                );
            if !has_side_effect && matches!(loc, AllocLoc::None | AllocLoc::RematConst(_)) {
                inst += 1;
                continue;
            }

            // 128-bit NEON SIMD vector fast path!
            if let Some((vsize, vfp)) = vec_ty {
                match op {
                    Op::Param => {}
                    Op::Uninit => {
                        let (vd, sp) = dst_vec(inst);
                        words.push(enc::movi_zero_v(vd));
                        if let Some(s) = sp {
                            emit_st_spill_vec(&mut words, vd, spill_base, s);
                        }
                    }
                    Op::Freeze => {
                        let (vd, sp) = dst_vec(inst);
                        let vs = load_vec(&mut words, at_u32(view.data, inst), V30);
                        if vd != vs {
                            words.push(enc::orr_v(vd, vs, vs));
                        }
                        if let Some(s) = sp {
                            emit_st_spill_vec(&mut words, vd, spill_base, s);
                        }
                    }
                    Op::Splat => {
                        let (vd, sp) = dst_vec(inst);
                        let src_v = at_u32(view.data, inst);
                        if !vfp && at_opt(&const_u64, to_idx(src_v)) == Some(0) {
                            words.push(enc::movi_zero_v(vd));
                        } else if vfp {
                            let vs = load_fp(&mut words, src_v, V30);
                            words.push(enc::dup_elem0_v(vsize, vd, vs));
                        } else {
                            let rs = load_gpr(&mut words, src_v, X16);
                            words.push(enc::dup_gen_v(vsize, vd, rs));
                        }
                        if let Some(s) = sp {
                            emit_st_spill_vec(&mut words, vd, spill_base, s);
                        }
                    }
                    Op::Load => {
                        let at = to_idx(at_u32(view.data, inst));
                        let (vd, sp) = dst_vec(inst);
                        match at_fusion(&fusion, inst) {
                            Fusion::MemSpOff { off } => emit_mem_q(&mut words, true, vd, SP, off),
                            Fusion::MemBaseOff { base, off, .. } => {
                                let rb = load_gpr(&mut words, base, X17);
                                emit_mem_q(&mut words, true, vd, rb, off);
                            }
                            Fusion::MemScaledIdx { base, idx, .. } => {
                                let rb = load_gpr(&mut words, base, X16);
                                let ri = load_gpr(&mut words, idx, X17);
                                words.push(enc::ldr_q_reg(vd, rb, ri, true));
                            }
                            Fusion::MemRegIdx { base, idx } => {
                                let rb = load_gpr(&mut words, base, X16);
                                let ri = load_gpr(&mut words, idx, X17);
                                words.push(enc::ldr_q_reg(vd, rb, ri, false));
                            }
                            _ => {
                                let ptr = at_u32(view.extra, at);
                                let off = i64::from(at_u32(view.extra, at + 1).cast_signed());
                                let rb = load_gpr(&mut words, ptr, X16);
                                emit_mem_q(&mut words, true, vd, rb, off);
                            }
                        }
                        if let Some(s) = sp {
                            emit_st_spill_vec(&mut words, vd, spill_base, s);
                        }
                    }
                    Op::Insert => {
                        // Insert scalar GPR into lane of `<4 x i32>` (`ins vd.s[lane], wn`) or `<2 x i64>` (`ins vd.d[lane], xn`).
                        let at = to_idx(at_u32(view.data, inst));
                        let vec_v = at_u32(view.extra, at);
                        let val_v = at_u32(view.extra, at + 1);
                        let lane = at_u32(view.extra, at + 2);
                        let (vd, sp) = dst_vec(inst);
                        let vs = load_vec(&mut words, vec_v, V30);
                        let rx = load_gpr(&mut words, val_v, X16);
                        if vd != vs {
                            words.push(enc::orr_v(vd, vs, vs));
                        }
                        if vsize == 2 {
                            words.push(enc::ins_s_gpr(vd, lane, rx));
                        } else {
                            words.push(enc::ins_d_gpr(vd, lane, rx));
                        }
                        if let Some(s) = sp {
                            emit_st_spill_vec(&mut words, vd, spill_base, s);
                        }
                    }
                    Op::Bitcast => {
                        let (vd, sp) = dst_vec(inst);
                        let vs = load_vec(&mut words, at_u32(view.data, inst), V30);
                        if vd != vs {
                            words.push(enc::orr_v(vd, vs, vs));
                        }
                        if let Some(s) = sp {
                            emit_st_spill_vec(&mut words, vd, spill_base, s);
                        }
                    }
                    Op::Umulhi => {
                        // `<4 x i32>` high-32-bit unsigned multiply: `umull.2d` + `umull2.2d` + `uzp2.4s`!
                        let at = to_idx(at_u32(view.data, inst));
                        let (vd, sp) = dst_vec(inst);
                        let va = load_vec(&mut words, at_u32(view.extra, at), V30);
                        let vb = load_vec(&mut words, at_u32(view.extra, at + 1), V31);
                        words.push(enc::umull_2d(V29, va, vb));
                        words.push(enc::umull2_2d(V30, va, vb));
                        words.push(enc::uzp2_4s(vd, V29, V30));
                        if let Some(s) = sp {
                            emit_st_spill_vec(&mut words, vd, spill_base, s);
                        }
                    }
                    Op::Shl | Op::Lshr => {
                        // Immediate lane shift on `<4 x i32>` (`shl.4s` / `ushr.4s`) or `<2 x i64>` (`shl.2d` / `ushr.2d`)!
                        let at = to_idx(at_u32(view.data, inst));
                        let src_v = at_u32(view.extra, at);
                        let sh_v = to_idx(at_u32(view.extra, at + 1));
                        let k = trunc_u32(at_opt(&const_u64, to_idx(at_u32(view.data, sh_v))).ok_or(TierError::Refused {
                            tier: TIER,
                            inst: Some(u32_of(inst)),
                            what: "non-const NEON shift",
                        })?);
                        let (vd, sp) = dst_vec(inst);
                        let vs = load_vec(&mut words, src_v, V30);
                        if k == 0 {
                            if vd != vs {
                                words.push(enc::orr_v(vd, vs, vs));
                            }
                        } else if vsize == 2 {
                            words.push(if op == Op::Shl { enc::shl_4s_imm(vd, vs, k) } else { enc::ushr_4s_imm(vd, vs, k) });
                        } else if op == Op::Shl {
                            words.push(enc::shl_2d_imm(vd, vs, k));
                        } else {
                            words.push(enc::ushr_2d_imm(vd, vs, k));
                        }
                        if let Some(s) = sp {
                            emit_st_spill_vec(&mut words, vd, spill_base, s);
                        }
                    }
                    Op::Add
                    | Op::Sub
                    | Op::Mul
                    | Op::And
                    | Op::Or
                    | Op::Xor
                    | Op::Umin
                    | Op::Umax
                    | Op::Smin
                    | Op::Smax
                    | Op::UaddSat
                    | Op::UsubSat
                    | Op::SaddSat
                    | Op::SsubSat => {
                        let (vd, sp) = dst_vec(inst);
                        if op == Op::Add {
                            if let Fusion::VecUadalp { acc, src_4s } = at_fusion(&fusion, inst) {
                                let vacc = load_vec(&mut words, acc, V30);
                                let vsrc = load_vec(&mut words, src_4s, V29);
                                if vd == vacc {
                                    words.push(enc::uadalp_2d(vd, vsrc));
                                } else if vd != vsrc {
                                    words.push(enc::orr_v(vd, vacc, vacc));
                                    words.push(enc::uadalp_2d(vd, vsrc));
                                } else {
                                    words.push(enc::orr_v(V31, vacc, vacc));
                                    words.push(enc::uadalp_2d(V31, vsrc));
                                    words.push(enc::orr_v(vd, V31, V31));
                                }
                                if let Some(s) = sp {
                                    emit_st_spill_vec(&mut words, vd, spill_base, s);
                                }
                                inst += 1;
                                continue;
                            }
                        }
                        if op == Op::Xor {
                            match at_fusion(&fusion, inst) {
                                Fusion::Bcax { a, b: mb, c } => {
                                    let va = load_vec(&mut words, a, V29);
                                    let vb = load_vec(&mut words, mb, V30);
                                    let vc = load_vec(&mut words, c, V31);
                                    words.push(enc::bcax_v(vd, va, vb, vc));
                                    if let Some(s) = sp {
                                        emit_st_spill_vec(&mut words, vd, spill_base, s);
                                    }
                                    inst += 1;
                                    continue;
                                }
                                Fusion::Rax1 { a, b: mb } => {
                                    let va = load_vec(&mut words, a, V30);
                                    let vb = load_vec(&mut words, mb, V31);
                                    words.push(enc::rax1_v(vd, va, vb));
                                    if let Some(s) = sp {
                                        emit_st_spill_vec(&mut words, vd, spill_base, s);
                                    }
                                    inst += 1;
                                    continue;
                                }
                                Fusion::Eor3 { a, b: mb, c } => {
                                    let va = load_vec(&mut words, a, V29);
                                    let vb = load_vec(&mut words, mb, V30);
                                    let vc = load_vec(&mut words, c, V31);
                                    words.push(enc::eor3_v(vd, va, vb, vc));
                                    if let Some(s) = sp {
                                        emit_st_spill_vec(&mut words, vd, spill_base, s);
                                    }
                                    inst += 1;
                                    continue;
                                }
                                _ => {}
                            }
                        }
                        let at = to_idx(at_u32(view.data, inst));
                        let va = load_vec(&mut words, at_u32(view.extra, at), V30);
                        let vb = load_vec(&mut words, at_u32(view.extra, at + 1), V31);
                        if op == Op::Mul && vsize == 3 {
                            // `<2 x i64>` widening 32x32->64 multiply (`xtn.2s` + `umull.2d`)!
                            words.push(enc::xtn_2s(V29, va));
                            words.push(enc::xtn_2s(V31, vb));
                            words.push(enc::umull_2d(vd, V29, V31));
                        } else {
                            let w = match op {
                                Op::Add => enc::add_v(vsize, vd, va, vb),
                                Op::Sub => enc::sub_v(vsize, vd, va, vb),
                                Op::Mul => enc::mul_v(vsize, vd, va, vb),
                                Op::And => enc::and_v(true, vd, va, vb),
                                Op::Or => enc::orr_v(vd, va, vb),
                                Op::Xor => enc::eor_v(vd, va, vb),
                                Op::Umin => enc::minmax_v(true, true, vsize, vd, va, vb),
                                Op::Umax => enc::minmax_v(true, false, vsize, vd, va, vb),
                                Op::Smin => enc::minmax_v(false, true, vsize, vd, va, vb),
                                Op::Smax => enc::minmax_v(false, false, vsize, vd, va, vb),
                                Op::UaddSat => enc::sat_addsub_v(true, false, vsize, vd, va, vb),
                                Op::UsubSat => enc::sat_addsub_v(true, true, vsize, vd, va, vb),
                                Op::SaddSat => enc::sat_addsub_v(false, false, vsize, vd, va, vb),
                                Op::SsubSat => enc::sat_addsub_v(false, true, vsize, vd, va, vb),
                                _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unsupported NEON binop" }),
                            };
                            words.push(w);
                        }
                        if let Some(s) = sp {
                            emit_st_spill_vec(&mut words, vd, spill_base, s);
                        }
                    }
                    Op::Fadd | Op::Fsub | Op::Fmul => {
                        let (vd, sp) = dst_vec(inst);
                        let double = vsize == 3;
                        if op == Op::Fadd {
                            if let Fusion::Fmadd { a, b: mb, acc } = at_fusion(&fusion, inst) {
                                let va = load_vec(&mut words, a, V29);
                                let vb = load_vec(&mut words, mb, V30);
                                let vc = load_vec(&mut words, acc, V31);
                                if vd == vc {
                                    // Zero-copy 1-instruction `fmla vd.2d, va.2d, vb.2d`!
                                    words.push(enc::fmla_v(double, vd, va, vb));
                                } else if vd != va && vd != vb {
                                    words.push(enc::orr_v(vd, vc, vc));
                                    words.push(enc::fmla_v(double, vd, va, vb));
                                } else {
                                    words.push(enc::orr_v(V31, vc, vc));
                                    words.push(enc::fmla_v(double, V31, va, vb));
                                    words.push(enc::orr_v(vd, V31, V31));
                                }
                                if let Some(s) = sp {
                                    emit_st_spill_vec(&mut words, vd, spill_base, s);
                                }
                                inst += 1;
                                continue;
                            }
                        }
                        let at = to_idx(at_u32(view.data, inst));
                        let va = load_vec(&mut words, at_u32(view.extra, at), V30);
                        let vb = load_vec(&mut words, at_u32(view.extra, at + 1), V31);
                        let w = match op {
                            Op::Fadd => enc::fadd_v(double, vd, va, vb),
                            Op::Fsub => enc::fsub_v(double, vd, va, vb),
                            Op::Fmul => enc::fmul_v(double, vd, va, vb),
                            _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unsupported NEON FP op" }),
                        };
                        words.push(w);
                        if let Some(s) = sp {
                            emit_st_spill_vec(&mut words, vd, spill_base, s);
                        }
                    }
                    Op::Select => {
                        let at = to_idx(at_u32(view.data, inst));
                        let (cv, tv, fv) = (at_u32(view.extra, at), at_u32(view.extra, at + 1), at_u32(view.extra, at + 2));
                        let (vd, sp) = dst_vec(inst);
                        let is_zero_splat = |v: u32| {
                            view.op(to_idx(v)) == Op::Splat && at_opt(&const_u64, to_idx(at_u32(view.data, to_idx(v)))) == Some(0)
                        };
                        let is_all_ones_splat = |v: u32| {
                            view.op(to_idx(v)) == Op::Splat && {
                                let src = to_idx(at_u32(view.data, to_idx(v)));
                                let elem_bits = ty_at(view.tys, src).bits(64);
                                let mask = if elem_bits >= 64 { u64::MAX } else { (1u64 << elem_bits) - 1 };
                                at_opt(&const_u64, src) == Some(mask)
                            }
                        };
                        if is_zero_splat(fv) && is_all_ones_splat(tv) {
                            // `Select(mask, all_ones, 0)` IS the NEON lane mask itself (0 ALU ops!).
                            let vm = load_vec(&mut words, cv, V30);
                            if vd != vm {
                                words.push(enc::orr_v(vd, vm, vm));
                            }
                        } else if is_zero_splat(fv) {
                            // `Select(mask, x, 0) -> and vd.16b, vx.16b, vmask.16b` (ONE instruction!)
                            let vm = load_vec(&mut words, cv, V30);
                            let vt = load_vec(&mut words, tv, V31);
                            words.push(enc::and_v(true, vd, vt, vm));
                        } else {
                            let vm = load_vec(&mut words, cv, V29);
                            let vt = load_vec(&mut words, tv, V30);
                            let vf = load_vec(&mut words, fv, V31);
                            if vd == vm {
                                words.push(enc::bsl_v(vd, vt, vf));
                            } else if vd == vf {
                                words.push(enc::bit_v(vd, vt, vm));
                            } else if vd == vt {
                                words.push(enc::bif_v(vd, vf, vm));
                            } else {
                                words.push(enc::orr_v(vd, vm, vm));
                                words.push(enc::bsl_v(vd, vt, vf));
                            }
                        }
                        if let Some(s) = sp {
                            emit_st_spill_vec(&mut words, vd, spill_base, s);
                        }
                    }
                    Op::Not => {
                        let (vd, sp) = dst_vec(inst);
                        let vs = load_vec(&mut words, at_u32(view.data, inst), V30);
                        words.push(enc::not_v(vd, vs));
                        if let Some(s) = sp {
                            emit_st_spill_vec(&mut words, vd, spill_base, s);
                        }
                    }
                    Op::Shuffle => {
                        // 16-byte NEON `tbl vd.16b, {va.16b}, vmask.16b` vectorized LUT shuffle!
                        let at = to_idx(at_u32(view.data, inst));
                        let a_v = at_u32(view.extra, at);
                        let (vd, sp) = dst_vec(inst);
                        let va = load_vec(&mut words, a_v, V30);
                        let mut lo64 = 0u64;
                        let mut hi64 = 0u64;
                        for k in 0..8usize {
                            lo64 |= u64::from(at_u32(view.extra, at + 3 + k) & 0xff) << (k * 8);
                            hi64 |= u64::from(at_u32(view.extra, at + 3 + 8 + k) & 0xff) << (k * 8);
                        }
                        emit_const_u64(&mut words, X16, lo64);
                        emit_const_u64(&mut words, X17, hi64);
                        words.push(enc::ins_d_gpr(V31, 0, X16));
                        words.push(enc::ins_d_gpr(V31, 1, X17));
                        words.push(enc::tbl1_v(vd, va, V31));
                        if let Some(s) = sp {
                            emit_st_spill_vec(&mut words, vd, spill_base, s);
                        }
                    }
                    Op::Rotl => {
                        // Fallback 128-bit `<2 x i64>` lane rotate when not fused into `rax1`.
                        let at = to_idx(at_u32(view.data, inst));
                        let src_v = at_u32(view.extra, at);
                        let sh_v = at_u32(view.extra, at + 1);
                        let (vd, sp) = dst_vec(inst);
                        let vs = load_vec(&mut words, src_v, V30);
                        let vsh = load_vec(&mut words, sh_v, V31);
                        for lane in 0..2u32 {
                            words.push(enc::umov_lane(3, lane, X16, vs));
                            words.push(enc::umov_lane(3, lane, X17, vsh));
                            words.push(enc::sub(X17, ZR, X17)); // ror(x, -k) == rol(x, k)
                            words.push(enc::rorv(X16, X16, X17));
                            words.push(enc::ins_d_gpr(vd, lane, X16));
                        }
                        if let Some(s) = sp {
                            emit_st_spill_vec(&mut words, vd, spill_base, s);
                        }
                    }
                    _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unsupported NEON op" }),
                }
                inst += 1;
                continue;
            }
            if op == Op::Store && is_vec128_ty(ty_at(view.tys, to_idx(at_u32(view.extra, to_idx(at_u32(view.data, inst)))))).is_some() {
                let at = to_idx(at_u32(view.data, inst));
                let vs = load_vec(&mut words, at_u32(view.extra, at), V30);
                match at_fusion(&fusion, inst) {
                    Fusion::MemSpOff { off } => emit_mem_q(&mut words, false, vs, SP, off),
                    Fusion::MemBaseOff { base, off, .. } => {
                        let rb = load_gpr(&mut words, base, X17);
                        emit_mem_q(&mut words, false, vs, rb, off);
                    }
                    Fusion::MemScaledIdx { base, idx, .. } => {
                        let rb = load_gpr(&mut words, base, X16);
                        let ri = load_gpr(&mut words, idx, X17);
                        words.push(enc::str_q_reg(vs, rb, ri, true));
                    }
                    Fusion::MemRegIdx { base, idx } => {
                        let rb = load_gpr(&mut words, base, X16);
                        let ri = load_gpr(&mut words, idx, X17);
                        words.push(enc::str_q_reg(vs, rb, ri, false));
                    }
                    _ => {
                        let ptr = at_u32(view.extra, at + 1);
                        let off = i64::from(at_u32(view.extra, at + 2).cast_signed());
                        let rb = load_gpr(&mut words, ptr, X16);
                        emit_mem_q(&mut words, false, vs, rb, off);
                    }
                }
                inst += 1;
                continue;
            }
            if op == Op::Store && ty_at(view.tys, to_idx(at_u32(view.extra, to_idx(at_u32(view.data, inst))))) == Ty::I128 {
                let at = to_idx(at_u32(view.data, inst));
                let val_v = at_u32(view.extra, at);
                let ptr_v = at_u32(view.extra, at + 1);
                let off = i64::from(at_u32(view.extra, at + 2).cast_signed());
                let v0 = match at_loc(&reg_alloc.loc_of, to_idx(val_v)) {
                    AllocLoc::GprPair(lo, _) => u32::from(lo),
                    AllocLoc::GprPairSpill(s0, _) => {
                        emit_ld_spill_gpr(&mut words, X16, spill_base, s0);
                        X16
                    }
                    _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unexpected i128 store val loc" }),
                };
                let rp = load_gpr(&mut words, ptr_v, X17);
                emit_mem_imm(&mut words, false, false, Size::X, v0, rp, off);
                let v1 = match at_loc(&reg_alloc.loc_of, to_idx(val_v)) {
                    AllocLoc::GprPair(_, hi) => u32::from(hi),
                    AllocLoc::GprPairSpill(_, s1) => {
                        emit_ld_spill_gpr(&mut words, X16, spill_base, s1);
                        X16
                    }
                    _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unexpected i128 store val loc" }),
                };
                let rp = load_gpr(&mut words, ptr_v, X17);
                emit_mem_imm(&mut words, false, false, Size::X, v1, rp, off + 8);
                inst += 1;
                continue;
            }
            if ty == Ty::I128 && !matches!(op, Op::Call | Op::CallIndirect) {
                let load_half = |words: &mut Vec<u32, Alloc<'a>>, v: u32, hi: bool, scratch: Reg| -> Reg {
                    match at_loc(&reg_alloc.loc_of, to_idx(v)) {
                        AllocLoc::GprPair(lo, h) => u32::from(if hi { h } else { lo }),
                        AllocLoc::GprPairSpill(s0, s1) => {
                            emit_ld_spill_gpr(words, scratch, spill_base, if hi { s1 } else { s0 });
                            scratch
                        }
                        _ => {
                            internal_err.set(Some(TierError::Refused { tier: TIER, inst: Some(v), what: "expected i128 pair value" }));
                            scratch
                        }
                    }
                };
                let (dlo, dhi, sp_pair) = match at_loc(&reg_alloc.loc_of, inst) {
                    AllocLoc::GprPair(lo, hi) => (u32::from(lo), u32::from(hi), None),
                    AllocLoc::GprPairSpill(s0, s1) => (X16, X17, Some((s0, s1))),
                    AllocLoc::None => (X16, X17, None),
                    _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "expected i128 pair dest" }),
                };
                match op {
                    Op::Param => {}
                    Op::Uninit => {
                        words.push(enc::mov(dlo, ZR));
                        if let Some((s0, _)) = sp_pair {
                            emit_st_spill_gpr(&mut words, dlo, spill_base, s0);
                        }
                        words.push(enc::mov(dhi, ZR));
                        if let Some((_, s1)) = sp_pair {
                            emit_st_spill_gpr(&mut words, dhi, spill_base, s1);
                        }
                    }
                    Op::Freeze => {
                        let a = at_u32(view.data, inst);
                        let a0 = load_half(&mut words, a, false, X16);
                        if dlo != a0 {
                            words.push(enc::mov(dlo, a0));
                        }
                        if let Some((s0, _)) = sp_pair {
                            emit_st_spill_gpr(&mut words, dlo, spill_base, s0);
                        }
                        let a1 = load_half(&mut words, a, true, X16);
                        if dhi != a1 {
                            words.push(enc::mov(dhi, a1));
                        }
                        if let Some((_, s1)) = sp_pair {
                            emit_st_spill_gpr(&mut words, dhi, spill_base, s1);
                        }
                    }
                    Op::Iconst => {
                        let v = i64::from(at_u32(view.data, inst).cast_signed());
                        emit_const_u64(&mut words, dlo, v.cast_unsigned());
                        if let Some((s0, _)) = sp_pair {
                            emit_st_spill_gpr(&mut words, dlo, spill_base, s0);
                        }
                        emit_const_u64(&mut words, dhi, (v >> 63).cast_unsigned());
                        if let Some((_, s1)) = sp_pair {
                            emit_st_spill_gpr(&mut words, dhi, spill_base, s1);
                        }
                    }
                    Op::IconstWide => {
                        let at = to_idx(at_u32(view.data, inst));
                        let lo = u64::from(at_u32(view.extra, at)) | (u64::from(at_u32(view.extra, at + 1)) << 32);
                        let hi = u64::from(at_u32(view.extra, at + 2)) | (u64::from(at_u32(view.extra, at + 3)) << 32);
                        emit_const_u64(&mut words, dlo, lo);
                        if let Some((s0, _)) = sp_pair {
                            emit_st_spill_gpr(&mut words, dlo, spill_base, s0);
                        }
                        emit_const_u64(&mut words, dhi, hi);
                        if let Some((_, s1)) = sp_pair {
                            emit_st_spill_gpr(&mut words, dhi, spill_base, s1);
                        }
                    }
                    Op::Add | Op::Sub | Op::And | Op::Or | Op::Xor => {
                        let at = to_idx(at_u32(view.data, inst));
                        let (a, bv) = (at_u32(view.extra, at), at_u32(view.extra, at + 1));
                        let a0 = load_half(&mut words, a, false, X16);
                        let b0 = load_half(&mut words, bv, false, X17);
                        let w0 = match op {
                            Op::Add => enc::adds(dlo, a0, b0),
                            Op::Sub => enc::subs(dlo, a0, b0),
                            Op::And => enc::and(dlo, a0, b0),
                            Op::Or => enc::orr(dlo, a0, b0),
                            _ => enc::eor(dlo, a0, b0),
                        };
                        words.push(w0);
                        if let Some((s0, _)) = sp_pair {
                            emit_st_spill_gpr(&mut words, dlo, spill_base, s0);
                        }
                        let a1 = load_half(&mut words, a, true, X16);
                        let b1 = load_half(&mut words, bv, true, X17);
                        let w1 = match op {
                            Op::Add => enc::adc(dhi, a1, b1),
                            Op::Sub => enc::sbc(dhi, a1, b1),
                            Op::And => enc::and(dhi, a1, b1),
                            Op::Or => enc::orr(dhi, a1, b1),
                            _ => enc::eor(dhi, a1, b1),
                        };
                        words.push(w1);
                        if let Some((_, s1)) = sp_pair {
                            emit_st_spill_gpr(&mut words, dhi, spill_base, s1);
                        }
                    }
                    Op::Neg | Op::Not => {
                        let a = at_u32(view.data, inst);
                        let a0 = load_half(&mut words, a, false, X16);
                        words.push(if op == Op::Neg { enc::subs(dlo, ZR, a0) } else { enc::mvn(dlo, a0) });
                        if let Some((s0, _)) = sp_pair {
                            emit_st_spill_gpr(&mut words, dlo, spill_base, s0);
                        }
                        let a1 = load_half(&mut words, a, true, X16);
                        words.push(if op == Op::Neg { enc::sbc(dhi, ZR, a1) } else { enc::mvn(dhi, a1) });
                        if let Some((_, s1)) = sp_pair {
                            emit_st_spill_gpr(&mut words, dhi, spill_base, s1);
                        }
                    }
                    Op::Mul => {
                        let at = to_idx(at_u32(view.data, inst));
                        let (a, bv) = (at_u32(view.extra, at), at_u32(view.extra, at + 1));
                        if let Some((s0, s1)) = sp_pair {
                            let a0 = load_half(&mut words, a, false, X16);
                            let b0 = load_half(&mut words, bv, false, X17);
                            words.push(enc::mul(X16, a0, b0));
                            emit_st_spill_gpr(&mut words, X16, spill_base, s0);
                            let a0 = load_half(&mut words, a, false, X16);
                            let b0 = load_half(&mut words, bv, false, X17);
                            words.push(enc::umulh(X16, a0, b0));
                            emit_st_spill_gpr(&mut words, X16, spill_base, s1);
                            let a0 = load_half(&mut words, a, false, X16);
                            let b1 = load_half(&mut words, bv, true, X17);
                            words.push(enc::mul(X16, a0, b1));
                            emit_ld_spill_gpr(&mut words, X17, spill_base, s1);
                            words.push(enc::add(X16, X17, X16));
                            emit_st_spill_gpr(&mut words, X16, spill_base, s1);
                            let a1 = load_half(&mut words, a, true, X16);
                            let b0 = load_half(&mut words, bv, false, X17);
                            words.push(enc::mul(X16, a1, b0));
                            emit_ld_spill_gpr(&mut words, X17, spill_base, s1);
                            words.push(enc::add(X16, X17, X16));
                            emit_st_spill_gpr(&mut words, X16, spill_base, s1);
                        } else {
                            let a0 = load_half(&mut words, a, false, X16);
                            let b0 = load_half(&mut words, bv, false, X17);
                            words.push(enc::umulh(dhi, a0, b0));
                            words.push(enc::mul(dlo, a0, b0));
                            let b1 = load_half(&mut words, bv, true, X17);
                            words.push(enc::madd(dhi, a0, b1, dhi));
                            let a1 = load_half(&mut words, a, true, X16);
                            let b0 = load_half(&mut words, bv, false, X17);
                            words.push(enc::madd(dhi, a1, b0, dhi));
                        }
                    }
                    Op::Udiv | Op::Sdiv | Op::Urem | Op::Srem => {
                        let at = to_idx(at_u32(view.data, inst));
                        let (a, bv) = (at_u32(view.extra, at), at_u32(view.extra, at + 1));
                        gpr_moves.clear();
                        for (val, dst_lo, dst_hi) in [(a, X0, X1), (bv, X2, X3)] {
                            match at_loc(&reg_alloc.loc_of, to_idx(val)) {
                                AllocLoc::GprPair(s0, s1) => {
                                    gpr_moves.push((GprCopyLoc::Reg(dst_lo), GprCopyLoc::Reg(u32::from(s0))));
                                    gpr_moves.push((GprCopyLoc::Reg(dst_hi), GprCopyLoc::Reg(u32::from(s1))));
                                }
                                AllocLoc::GprPairSpill(s0, s1) => {
                                    gpr_moves.push((GprCopyLoc::Reg(dst_lo), GprCopyLoc::Spill(s0)));
                                    gpr_moves.push((GprCopyLoc::Reg(dst_hi), GprCopyLoc::Spill(s1)));
                                }
                                _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unexpected i128 div operand loc" }),
                            }
                        }
                        emit_parallel_gpr_moves(&mut words, &mut gpr_moves, spill_base)?;
                        // Trap on zero divisor (`BRK_DIV_ZERO`).
                        words.push(enc::orr(X9, X2, X3));
                        words.push(enc::cbnz(X9, 2));
                        words.push(enc::brk(BRK_DIV_ZERO));
                        if op == Op::Sdiv {
                            // Trap on `i128::MIN / -1` (`BRK_DIV_OVERFLOW`).
                            words.push(enc::and(X9, X2, X3));
                            words.push(enc::cmn_imm(X9, 1));
                            let w_ok = words.len();
                            words.push(enc::b_cond(Cond::Ne, 0));
                            words.push(enc::movz(X9, 0x8000, 3));
                            words.push(enc::eor(X9, X1, X9));
                            words.push(enc::orr(X9, X9, X0));
                            words.push(enc::cbnz(X9, 2));
                            words.push(enc::brk(BRK_DIV_OVERFLOW));
                            let ok_at = words.len();
                            set_at(&mut words, w_ok, enc::b_cond(Cond::Ne, i32_of(ok_at - w_ok)));
                        }
                        let rname = match op {
                            Op::Udiv => "__udivti3",
                            Op::Urem => "__umodti3",
                            Op::Sdiv => "__divti3",
                            _ => "__modti3",
                        };
                        let target = resolve_c_routine(rname, &mut out);
                        emit_routine_call(&mut words, target, call_mode, &mut out);
                        if let Some((s0, s1)) = sp_pair {
                            emit_st_spill_gpr(&mut words, X0, spill_base, s0);
                            emit_st_spill_gpr(&mut words, X1, spill_base, s1);
                        } else {
                            gpr_moves.clear();
                            gpr_moves.push((GprCopyLoc::Reg(dlo), GprCopyLoc::Reg(X0)));
                            gpr_moves.push((GprCopyLoc::Reg(dhi), GprCopyLoc::Reg(X1)));
                            emit_parallel_gpr_moves(&mut words, &mut gpr_moves, spill_base)?;
                        }
                    }
                    Op::Shl | Op::Lshr | Op::Ashr => {
                        let at = to_idx(at_u32(view.data, inst));
                        let (a, bv) = (at_u32(view.extra, at), at_u32(view.extra, at + 1));
                        let const_sh: Option<u32> = if let Some(c) = at_opt(&const_u64, to_idx(bv)) {
                            Some(trunc_u32(c & 127))
                        } else if view.op(to_idx(bv)) == Op::Iconst {
                            Some(at_u32(view.data, to_idx(bv)) & 127)
                        } else if view.op(to_idx(bv)) == Op::IconstWide {
                            Some(at_u32(view.extra, to_idx(at_u32(view.data, to_idx(bv)))) & 127)
                        } else {
                            None
                        };
                        if let Some(k) = const_sh {
                            if k == 0 {
                                let a0 = load_half(&mut words, a, false, X16);
                                if dlo != a0 {
                                    words.push(enc::mov(dlo, a0));
                                }
                                if let Some((s0, _)) = sp_pair {
                                    emit_st_spill_gpr(&mut words, dlo, spill_base, s0);
                                }
                                let a1 = load_half(&mut words, a, true, X16);
                                if dhi != a1 {
                                    words.push(enc::mov(dhi, a1));
                                }
                                if let Some((_, s1)) = sp_pair {
                                    emit_st_spill_gpr(&mut words, dhi, spill_base, s1);
                                }
                            } else if op == Op::Shl {
                                if k >= 64 {
                                    let a0 = load_half(&mut words, a, false, X16);
                                    if k == 64 {
                                        if dhi != a0 {
                                            words.push(enc::mov(dhi, a0));
                                        }
                                    } else {
                                        words.push(enc::lsl_imm(dhi, a0, k - 64));
                                    }
                                    if let Some((_, s1)) = sp_pair {
                                        emit_st_spill_gpr(&mut words, dhi, spill_base, s1);
                                    }
                                    words.push(enc::mov(dlo, ZR));
                                    if let Some((s0, _)) = sp_pair {
                                        emit_st_spill_gpr(&mut words, dlo, spill_base, s0);
                                    }
                                } else {
                                    let a1 = load_half(&mut words, a, true, X16);
                                    words.push(enc::lsl_imm(X16, a1, k));
                                    let a0 = load_half(&mut words, a, false, X17);
                                    words.push(enc::lsr_imm(X17, a0, 64 - k));
                                    words.push(enc::orr(dhi, X16, X17));
                                    if let Some((_, s1)) = sp_pair {
                                        emit_st_spill_gpr(&mut words, dhi, spill_base, s1);
                                    }
                                    let a0 = load_half(&mut words, a, false, X16);
                                    words.push(enc::lsl_imm(dlo, a0, k));
                                    if let Some((s0, _)) = sp_pair {
                                        emit_st_spill_gpr(&mut words, dlo, spill_base, s0);
                                    }
                                }
                            } else {
                                let arith = op == Op::Ashr;
                                if k >= 64 {
                                    let a1 = load_half(&mut words, a, true, X16);
                                    if k == 64 {
                                        if dlo != a1 {
                                            words.push(enc::mov(dlo, a1));
                                        }
                                    } else {
                                        words.push(if arith { enc::asr_imm(dlo, a1, k - 64) } else { enc::lsr_imm(dlo, a1, k - 64) });
                                    }
                                    if let Some((s0, _)) = sp_pair {
                                        emit_st_spill_gpr(&mut words, dlo, spill_base, s0);
                                    }
                                    if arith {
                                        let a1 = load_half(&mut words, a, true, X16);
                                        words.push(enc::asr_imm(dhi, a1, 63));
                                    } else {
                                        words.push(enc::mov(dhi, ZR));
                                    }
                                    if let Some((_, s1)) = sp_pair {
                                        emit_st_spill_gpr(&mut words, dhi, spill_base, s1);
                                    }
                                } else {
                                    let a0 = load_half(&mut words, a, false, X16);
                                    words.push(enc::lsr_imm(X16, a0, k));
                                    let a1 = load_half(&mut words, a, true, X17);
                                    words.push(enc::lsl_imm(X17, a1, 64 - k));
                                    words.push(enc::orr(dlo, X16, X17));
                                    if let Some((s0, _)) = sp_pair {
                                        emit_st_spill_gpr(&mut words, dlo, spill_base, s0);
                                    }
                                    let a1 = load_half(&mut words, a, true, X16);
                                    words.push(if arith { enc::asr_imm(dhi, a1, k) } else { enc::lsr_imm(dhi, a1, k) });
                                    if let Some((_, s1)) = sp_pair {
                                        emit_st_spill_gpr(&mut words, dhi, spill_base, s1);
                                    }
                                }
                            }
                        } else {
                            // General runtime shift amount `n & 127` using `tbnz #6` (only 2 scratch GPRs `X16, X17`).
                            let rb = if ty_at(view.tys, to_idx(bv)) == Ty::I128 {
                                load_half(&mut words, bv, false, X16)
                            } else {
                                load_gpr(&mut words, bv, X16)
                            };
                            words.push(enc::and_imm(X16, rb, 1, 0, 6)); // n & 127 in X16
                            let br_ge64 = words.len();
                            words.push(enc::tbnz(X16, 6, 0));
                            if let Some((s0, s1)) = sp_pair {
                                emit_st_spill_gpr(&mut words, X16, spill_base, s1); // save n temporarily in s1
                                if op == Op::Shl {
                                    let a0 = load_half(&mut words, a, false, X17);
                                    words.push(enc::lslv(X17, a0, X16));
                                    emit_st_spill_gpr(&mut words, X17, spill_base, s0);
                                    let a0 = load_half(&mut words, a, false, X17);
                                    words.push(enc::lsr_imm(X17, a0, 1));
                                    words.push(enc::mvn(X16, X16)); // 63 - n
                                    words.push(enc::lsrv(X17, X17, X16));
                                    // Reload n into X16, combine with a1 << n.
                                    emit_ld_spill_gpr(&mut words, X16, spill_base, s1);
                                    if let AllocLoc::GprPair(_, h) = at_loc(&reg_alloc.loc_of, to_idx(a)) {
                                        words.push(enc::lslv(X16, u32::from(h), X16));
                                        words.push(enc::orr(X16, X16, X17));
                                    } else {
                                        // Both a and dst spilled: swap via s1 carefully.
                                        emit_st_spill_gpr(&mut words, X17, spill_base, s1); // store carry-in in s1
                                        let a1 = load_half(&mut words, a, true, X17);
                                        words.push(enc::lslv(X16, a1, X16));
                                        emit_ld_spill_gpr(&mut words, X17, spill_base, s1);
                                        words.push(enc::orr(X16, X16, X17));
                                    }
                                    emit_st_spill_gpr(&mut words, X16, spill_base, s1);
                                } else {
                                    let arith = op == Op::Ashr;
                                    let a1 = load_half(&mut words, a, true, X17);
                                    words.push(enc::lsl_imm(X17, a1, 1));
                                    words.push(enc::mvn(X16, X16));
                                    words.push(enc::lslv(X17, X17, X16));
                                    emit_st_spill_gpr(&mut words, X17, spill_base, s0);
                                    emit_ld_spill_gpr(&mut words, X16, spill_base, s1);
                                    let a0 = load_half(&mut words, a, false, X17);
                                    words.push(enc::lsrv(X17, a0, X16));
                                    emit_ld_spill_gpr(&mut words, X16, spill_base, s0);
                                    words.push(enc::orr(X16, X17, X16));
                                    emit_st_spill_gpr(&mut words, X16, spill_base, s0);
                                    emit_ld_spill_gpr(&mut words, X16, spill_base, s1);
                                    let a1 = load_half(&mut words, a, true, X17);
                                    words.push(if arith { enc::asrv(X16, a1, X16) } else { enc::lsrv(X16, a1, X16) });
                                    emit_st_spill_gpr(&mut words, X16, spill_base, s1);
                                }
                                let br_done = words.len();
                                words.push(enc::b(0));
                                let ge64_at = words.len();
                                set_at(&mut words, br_ge64, enc::tbnz(X16, 6, i32_of(ge64_at - br_ge64)));
                                if op == Op::Shl {
                                    let a0 = load_half(&mut words, a, false, X17);
                                    words.push(enc::lslv(X17, a0, X16));
                                    emit_st_spill_gpr(&mut words, X17, spill_base, s1);
                                    emit_st_spill_gpr(&mut words, ZR, spill_base, s0);
                                } else {
                                    let arith = op == Op::Ashr;
                                    let a1 = load_half(&mut words, a, true, X17);
                                    words.push(if arith { enc::asrv(X16, a1, X16) } else { enc::lsrv(X16, a1, X16) });
                                    emit_st_spill_gpr(&mut words, X16, spill_base, s0);
                                    if arith {
                                        let a1 = load_half(&mut words, a, true, X17);
                                        words.push(enc::asr_imm(X17, a1, 63));
                                        emit_st_spill_gpr(&mut words, X17, spill_base, s1);
                                    } else {
                                        emit_st_spill_gpr(&mut words, ZR, spill_base, s1);
                                    }
                                }
                                let done_at = words.len();
                                set_at(&mut words, br_done, enc::b(i32_of(done_at - br_done)));
                            } else {
                                if op == Op::Shl {
                                    let a0 = load_half(&mut words, a, false, X17);
                                    words.push(enc::lslv(dlo, a0, X16));
                                    words.push(enc::lsr_imm(X17, a0, 1));
                                    words.push(enc::mvn(dhi, X16));
                                    words.push(enc::lsrv(X17, X17, dhi));
                                    let a1 = load_half(&mut words, a, true, dhi);
                                    words.push(enc::lslv(dhi, a1, X16));
                                    words.push(enc::orr(dhi, dhi, X17));
                                } else {
                                    let arith = op == Op::Ashr;
                                    let a1 = load_half(&mut words, a, true, X17);
                                    words.push(enc::lsl_imm(X17, a1, 1));
                                    words.push(enc::mvn(dlo, X16));
                                    words.push(enc::lslv(X17, X17, dlo));
                                    let a0 = load_half(&mut words, a, false, dlo);
                                    words.push(enc::lsrv(dlo, a0, X16));
                                    words.push(enc::orr(dlo, dlo, X17));
                                    let a1 = load_half(&mut words, a, true, X17);
                                    words.push(if arith { enc::asrv(dhi, a1, X16) } else { enc::lsrv(dhi, a1, X16) });
                                }
                                let br_done = words.len();
                                words.push(enc::b(0));
                                let ge64_at = words.len();
                                set_at(&mut words, br_ge64, enc::tbnz(X16, 6, i32_of(ge64_at - br_ge64)));
                                if op == Op::Shl {
                                    let a0 = load_half(&mut words, a, false, X17);
                                    words.push(enc::lslv(dhi, a0, X16));
                                    words.push(enc::mov(dlo, ZR));
                                } else {
                                    let arith = op == Op::Ashr;
                                    let a1 = load_half(&mut words, a, true, X17);
                                    words.push(if arith { enc::asrv(dlo, a1, X16) } else { enc::lsrv(dlo, a1, X16) });
                                    if arith {
                                        words.push(enc::asr_imm(dhi, a1, 63));
                                    } else {
                                        words.push(enc::mov(dhi, ZR));
                                    }
                                }
                                let done_at = words.len();
                                set_at(&mut words, br_done, enc::b(i32_of(done_at - br_done)));
                            }
                        }
                    }
                    Op::Zext => {
                        let src_v = at_u32(view.data, inst);
                        let rs = load_gpr(&mut words, src_v, X16);
                        if dlo != rs {
                            words.push(enc::mov(dlo, rs));
                        }
                        if let Some((s0, _)) = sp_pair {
                            emit_st_spill_gpr(&mut words, dlo, spill_base, s0);
                        }
                        words.push(enc::mov(dhi, ZR));
                        if let Some((_, s1)) = sp_pair {
                            emit_st_spill_gpr(&mut words, dhi, spill_base, s1);
                        }
                    }
                    Op::Sext => {
                        let src_v = at_u32(view.data, inst);
                        let src_bits = ty_at(view.tys, to_idx(src_v)).bits(64);
                        let rs = load_gpr(&mut words, src_v, X16);
                        if src_bits < 64 {
                            words.push(enc::sext(dlo, rs, src_bits));
                        } else if dlo != rs {
                            words.push(enc::mov(dlo, rs));
                        }
                        if let Some((s0, _)) = sp_pair {
                            emit_st_spill_gpr(&mut words, dlo, spill_base, s0);
                        }
                        words.push(enc::asr_imm(dhi, dlo, 63));
                        if let Some((_, s1)) = sp_pair {
                            emit_st_spill_gpr(&mut words, dhi, spill_base, s1);
                        }
                    }
                    Op::Select => {
                        let at = to_idx(at_u32(view.data, inst));
                        let (cv, tv, fv) = (at_u32(view.extra, at), at_u32(view.extra, at + 1), at_u32(view.extra, at + 2));
                        let rc = load_gpr(&mut words, cv, X16);
                        words.push(enc::cmp_imm(rc, 0));
                        let t0 = load_half(&mut words, tv, false, X16);
                        let f0 = load_half(&mut words, fv, false, X17);
                        words.push(enc::csel(dlo, t0, f0, Cond::Ne));
                        if let Some((s0, _)) = sp_pair {
                            emit_st_spill_gpr(&mut words, dlo, spill_base, s0);
                        }
                        let t1 = load_half(&mut words, tv, true, X16);
                        let f1 = load_half(&mut words, fv, true, X17);
                        words.push(enc::csel(dhi, t1, f1, Cond::Ne));
                        if let Some((_, s1)) = sp_pair {
                            emit_st_spill_gpr(&mut words, dhi, spill_base, s1);
                        }
                    }
                    Op::Load => {
                        let at = to_idx(at_u32(view.data, inst));
                        let ptr_v = at_u32(view.extra, at);
                        let off = i64::from(at_u32(view.extra, at + 1).cast_signed());
                        let rp = load_gpr(&mut words, ptr_v, X17);
                        emit_mem_imm(&mut words, true, false, Size::X, dlo, rp, off);
                        if let Some((s0, _)) = sp_pair {
                            emit_st_spill_gpr(&mut words, dlo, spill_base, s0);
                        }
                        let rp = load_gpr(&mut words, ptr_v, X17);
                        emit_mem_imm(&mut words, true, false, Size::X, dhi, rp, off + 8);
                        if let Some((_, s1)) = sp_pair {
                            emit_st_spill_gpr(&mut words, dhi, spill_base, s1);
                        }
                    }
                    Op::UaddSat | Op::UsubSat | Op::SaddSat | Op::SsubSat => {
                        let at = to_idx(at_u32(view.data, inst));
                        let (a, bv) = (at_u32(view.extra, at), at_u32(view.extra, at + 1));
                        let a0 = load_half(&mut words, a, false, X16);
                        let b0 = load_half(&mut words, bv, false, X17);
                        let is_add = matches!(op, Op::UaddSat | Op::SaddSat);
                        words.push(if is_add { enc::adds(dlo, a0, b0) } else { enc::subs(dlo, a0, b0) });
                        if let Some((s0, _)) = sp_pair {
                            emit_st_spill_gpr(&mut words, dlo, spill_base, s0);
                        }
                        let a1 = load_half(&mut words, a, true, X16);
                        let b1 = load_half(&mut words, bv, true, X17);
                        words.push(if is_add { enc::adcs(dhi, a1, b1) } else { enc::sbcs(dhi, a1, b1) });
                        if let Some((_, s1)) = sp_pair {
                            emit_st_spill_gpr(&mut words, dhi, spill_base, s1);
                        }
                        match op {
                            Op::UaddSat => {
                                words.push(enc::movn(X16, 0, 0));
                                let cur_lo = if let Some((s0, _)) = sp_pair { emit_ld_spill_gpr(&mut words, X17, spill_base, s0); X17 } else { dlo };
                                words.push(enc::csel(dlo, X16, cur_lo, Cond::Hs));
                                if let Some((s0, _)) = sp_pair { emit_st_spill_gpr(&mut words, dlo, spill_base, s0); }
                                let cur_hi = if let Some((_, s1)) = sp_pair { emit_ld_spill_gpr(&mut words, X17, spill_base, s1); X17 } else { dhi };
                                words.push(enc::csel(dhi, X16, cur_hi, Cond::Hs));
                                if let Some((_, s1)) = sp_pair { emit_st_spill_gpr(&mut words, dhi, spill_base, s1); }
                            }
                            Op::UsubSat => {
                                let cur_lo = if let Some((s0, _)) = sp_pair { emit_ld_spill_gpr(&mut words, X17, spill_base, s0); X17 } else { dlo };
                                words.push(enc::csel(dlo, ZR, cur_lo, Cond::Lo));
                                if let Some((s0, _)) = sp_pair { emit_st_spill_gpr(&mut words, dlo, spill_base, s0); }
                                let cur_hi = if let Some((_, s1)) = sp_pair { emit_ld_spill_gpr(&mut words, X17, spill_base, s1); X17 } else { dhi };
                                words.push(enc::csel(dhi, ZR, cur_hi, Cond::Lo));
                                if let Some((_, s1)) = sp_pair { emit_st_spill_gpr(&mut words, dhi, spill_base, s1); }
                            }
                            Op::SaddSat | Op::SsubSat => {
                                let a1 = load_half(&mut words, a, true, X16);
                                words.push(enc::asr_imm(X16, a1, 63));
                                words.push(enc::mvn(X16, X16)); // !m = bound_lo
                                let cur_lo = if let Some((s0, _)) = sp_pair { emit_ld_spill_gpr(&mut words, X17, spill_base, s0); X17 } else { dlo };
                                words.push(enc::csel(dlo, X16, cur_lo, Cond::Vs));
                                if let Some((s0, _)) = sp_pair { emit_st_spill_gpr(&mut words, dlo, spill_base, s0); }
                                // bound_hi = bound_lo ^ (1 << 63) (none of movz/eor touch NZCV!).
                                words.push(enc::movz(X17, 0x8000, 3));
                                words.push(enc::eor(X16, X16, X17));
                                let cur_hi = if let Some((_, s1)) = sp_pair { emit_ld_spill_gpr(&mut words, X17, spill_base, s1); X17 } else { dhi };
                                words.push(enc::csel(dhi, X16, cur_hi, Cond::Vs));
                                if let Some((_, s1)) = sp_pair { emit_st_spill_gpr(&mut words, dhi, spill_base, s1); }
                            }
                            _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unsupported i128 sat op" }),
                        }
                    }
                    _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unsupported i128 op" }),
                }
                inst += 1;
                continue;
            }
            if matches!(
                op,
                Op::ReduceAdd
                    | Op::ReduceAnd
                    | Op::ReduceOr
                    | Op::ReduceXor
                    | Op::ReduceUmin
                    | Op::ReduceUmax
                    | Op::ReduceSmin
                    | Op::ReduceSmax
                    | Op::ReduceFadd
            ) {
                if op == Op::ReduceFadd {
                    let at = to_idx(at_u32(view.data, inst));
                    let start_v = at_u32(view.extra, at);
                    let vec_v = at_u32(view.extra, at + 1);
                    let (vsize, _) = is_vec128_ty(ty_at(view.tys, to_idx(vec_v))).ok_or(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "non-vec128 type" })?;
                    let double = vsize == 3;
                    let (vd, sp) = dst_fp(inst);
                    let vs = load_vec(&mut words, vec_v, V30);
                    let vstart = load_fp(&mut words, start_v, V29);
                    if double {
                        words.push(enc::faddp_scalar(true, V31, vs));
                    } else {
                        words.push(enc::faddp_v(false, V31, vs, vs));
                        words.push(enc::faddp_scalar(false, V31, V31));
                    }
                    words.push(enc::fbin(0b0010, double, vd, vstart, V31));
                    if let Some(s) = sp {
                        emit_st_spill_fp(&mut words, vd, spill_base, s);
                    }
                } else {
                    let vec_v = at_u32(view.data, inst);
                    let (vsize, _) = is_vec128_ty(ty_at(view.tys, to_idx(vec_v))).ok_or(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "non-vec128 type" })?;
                    let (rd, sp) = dst_gpr(inst);
                    let vs = load_vec(&mut words, vec_v, V30);
                    match op {
                        Op::ReduceAdd => {
                            if vsize == 3 {
                                words.push(enc::addp_d(V31, vs));
                                words.push(enc::fmov_from_d(rd, V31));
                            } else {
                                words.push(enc::addv_v(vsize, V31, vs));
                                words.push(enc::umov_lane(vsize, 0, rd, V31));
                            }
                        }
                        Op::ReduceUmin | Op::ReduceUmax | Op::ReduceSmin | Op::ReduceSmax => {
                            let unsigned = matches!(op, Op::ReduceUmin | Op::ReduceUmax);
                            let is_min = matches!(op, Op::ReduceUmin | Op::ReduceSmin);
                            if vsize < 3 {
                                words.push(enc::minmaxv_v(unsigned, is_min, vsize, V31, vs));
                                words.push(enc::umov_lane(vsize, 0, rd, V31));
                            } else {
                                words.push(enc::umov_lane(3, 0, X16, vs));
                                words.push(enc::umov_lane(3, 1, X17, vs));
                                words.push(enc::cmp(X16, X17));
                                let cond = match (unsigned, is_min) {
                                    (true, true) => Cond::Lo,
                                    (true, false) => Cond::Hi,
                                    (false, true) => Cond::Lt,
                                    (false, false) => Cond::Gt,
                                };
                                words.push(enc::csel(rd, X16, X17, cond));
                            }
                        }
                        Op::ReduceXor | Op::ReduceOr | Op::ReduceAnd => {
                            let elem_bytes = 1u32 << vsize;
                            let mut cur = vs;
                            let mut step = 8u32;
                            while step >= elem_bytes {
                                words.push(enc::ext_v(V31, cur, cur, step));
                                let comb = match op {
                                    Op::ReduceXor => enc::eor_v(V30, cur, V31),
                                    Op::ReduceOr => enc::orr_v(V30, cur, V31),
                                    _ => enc::and_v(true, V30, cur, V31),
                                };
                                words.push(comb);
                                cur = V30;
                                step >>= 1;
                            }
                            words.push(enc::umov_lane(vsize, 0, rd, cur));
                        }
                        _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unsupported vector reduce op" }),
                    }
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                inst += 1;
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
                    let val = at_opt(&const_u64, inst).unwrap_or(0);
                    emit_const_u64(&mut words, rd, val);
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Fconst => {
                    let (vd, sp) = dst_fp(inst);
                    let fbits = at_u32(view.data, inst);
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
                    let at = to_idx(at_u32(view.data, inst));
                    let fbits = u64::from(at_u32(view.extra, at)) | (u64::from(at_u32(view.extra, at + 1)) << 32);
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
                        let vs = load_fp(&mut words, at_u32(view.data, inst), V30);
                        if vd != vs {
                            words.push(enc::fmov_reg(double, vd, vs));
                        }
                        if let Some(s) = sp {
                            emit_st_spill_fp(&mut words, vd, spill_base, s);
                        }
                    } else {
                        let (rd, sp) = dst_gpr(inst);
                        let src = load_gpr(&mut words, at_u32(view.data, inst), X16);
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
                    let src = load_gpr(&mut words, at_u32(view.data, inst), X16);
                    if rd != src {
                        words.push(enc::mov(rd, src));
                    }
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::PtrWithAddr => {
                    let at = to_idx(at_u32(view.data, inst));
                    let addr = at_u32(view.extra, at + 1);
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
                    let off = at_u32(&slot_at, to_idx(at_u32(view.data, inst)));
                    emit_sp_addr(&mut words, rd, off);
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::PtrAdd => {
                    let at = to_idx(at_u32(view.data, inst));
                    let ptr = at_u32(view.extra, at);
                    let by = at_u32(view.extra, at + 1);
                    let (rd, sp) = dst_gpr(inst);
                    let rptr = load_gpr(&mut words, ptr, X16);
                    if let Some(cv) = at_opt(&const_u64, to_idx(by)) {
                        let by_bits = ty_at(view.tys, to_idx(by)).bits(64);
                        let delta = sign_extend_u64(cv, by_bits);
                        if (0..=4095).contains(&delta) {
                            words.push(enc::add_imm(rd, rptr, u32_of(delta), false));
                        } else if (-4095..0).contains(&delta) {
                            words.push(enc::sub_imm(rd, rptr, u32_of(-delta), false));
                        } else {
                            emit_const_u64(&mut words, X17, delta.cast_unsigned());
                            words.push(enc::add(rd, rptr, X17));
                        }
                    } else {
                        let by_bits = ty_at(view.tys, to_idx(by)).bits(64);
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
                    let at = to_idx(at_u32(view.data, inst));
                    let (size, is_fp) = mem_size_of(ty).ok_or(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unsupported mem size" })?;
                    let (dst, sp) = if is_fp { dst_fp(inst) } else { dst_gpr(inst) };
                    match at_fusion(&fusion, inst) {
                        Fusion::MemSpOff { off } => {
                            emit_mem_imm(&mut words, true, is_fp, size, dst, SP, off);
                        }
                        Fusion::MemBaseOff { base, off, .. } => {
                            let rbase = load_gpr(&mut words, base, X17);
                            emit_mem_imm(&mut words, true, is_fp, size, dst, rbase, off);
                        }
                        Fusion::MemScaledIdx { base, idx, .. } => {
                            let rbase = load_gpr(&mut words, base, X16);
                            let ridx = load_gpr(&mut words, idx, X17);
                            let raw = enc::ldr_reg_scaled(size, dst, rbase, ridx);
                            words.push(if is_fp { enc::fp_access(raw) } else { raw });
                        }
                        Fusion::MemRegIdx { base, idx } => {
                            let rbase = load_gpr(&mut words, base, X16);
                            let ridx = load_gpr(&mut words, idx, X17);
                            let raw = enc::ldr_reg(size, dst, rbase, ridx);
                            words.push(if is_fp { enc::fp_access(raw) } else { raw });
                        }
                        _ => {
                            let ptr = at_u32(view.extra, at);
                            let off = i64::from(at_u32(view.extra, at + 1).cast_signed());
                            if fits_mem_off(size, off) {
                                let rbase = load_gpr(&mut words, ptr, X17);
                                emit_mem_imm(&mut words, true, is_fp, size, dst, rbase, off);
                            } else {
                                let rptr = load_gpr(&mut words, ptr, X16);
                                emit_const_u64(&mut words, X17, off.cast_unsigned());
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
                    let at = to_idx(at_u32(view.data, inst));
                    let val = at_u32(view.extra, at);
                    let val_ty = ty_at(view.tys, to_idx(val));
                    let (size, is_fp) = mem_size_of(val_ty).ok_or(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unsupported mem size" })?;
                    match at_fusion(&fusion, inst) {
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
                        Fusion::MemScaledIdx { base, idx, .. } => {
                            if is_fp {
                                let rval = load_fp(&mut words, val, V30);
                                let rbase = load_gpr(&mut words, base, X16);
                                let ridx = load_gpr(&mut words, idx, X17);
                                words.push(enc::fp_access(enc::str_reg_scaled(size, rval, rbase, ridx)));
                            } else {
                                let spilled = [val, base, idx]
                                    .iter()
                                    .filter(|&&v| !matches!(at_loc(&reg_alloc.loc_of, to_idx(v)), AllocLoc::Gpr(_)))
                                    .count();
                                if spilled <= 2 {
                                    let mut next_s = X16;
                                    let mut grab = |words: &mut Vec<u32, Alloc<'a>>, v: u32| -> Reg {
                                        match at_loc(&reg_alloc.loc_of, to_idx(v)) {
                                            AllocLoc::Gpr(r) => u32::from(r),
                                            _ => {
                                                let s = next_s;
                                                next_s = X17;
                                                load_gpr(words, v, s)
                                            }
                                        }
                                    };
                                    let rval = grab(&mut words, val);
                                    let rbase = grab(&mut words, base);
                                    let ridx = grab(&mut words, idx);
                                    words.push(enc::str_reg_scaled(size, rval, rbase, ridx));
                                } else {
                                    let rbase = load_gpr(&mut words, base, X16);
                                    let ridx = load_gpr(&mut words, idx, X17);
                                    words.push(enc::add_lsl(X17, rbase, ridx, mem_shift(size)));
                                    let rval = load_gpr(&mut words, val, X16);
                                    emit_mem_imm(&mut words, false, false, size, rval, X17, 0);
                                }
                            }
                        }
                        Fusion::MemRegIdx { base, idx } => {
                            if is_fp {
                                let rval = load_fp(&mut words, val, V30);
                                let rbase = load_gpr(&mut words, base, X16);
                                let ridx = load_gpr(&mut words, idx, X17);
                                words.push(enc::fp_access(enc::str_reg(size, rval, rbase, ridx)));
                            } else {
                                let spilled = [val, base, idx]
                                    .iter()
                                    .filter(|&&v| !matches!(at_loc(&reg_alloc.loc_of, to_idx(v)), AllocLoc::Gpr(_)))
                                    .count();
                                if spilled <= 2 {
                                    let mut next_s = X16;
                                    let mut grab = |words: &mut Vec<u32, Alloc<'a>>, v: u32| -> Reg {
                                        match at_loc(&reg_alloc.loc_of, to_idx(v)) {
                                            AllocLoc::Gpr(r) => u32::from(r),
                                            _ => {
                                                let s = next_s;
                                                next_s = X17;
                                                load_gpr(words, v, s)
                                            }
                                        }
                                    };
                                    let rval = grab(&mut words, val);
                                    let rbase = grab(&mut words, base);
                                    let ridx = grab(&mut words, idx);
                                    words.push(enc::str_reg(size, rval, rbase, ridx));
                                } else {
                                    let rbase = load_gpr(&mut words, base, X16);
                                    let ridx = load_gpr(&mut words, idx, X17);
                                    words.push(enc::add(X17, rbase, ridx));
                                    let rval = load_gpr(&mut words, val, X16);
                                    emit_mem_imm(&mut words, false, false, size, rval, X17, 0);
                                }
                            }
                        }
                        _ => {
                            let ptr = at_u32(view.extra, at + 1);
                            let off = i64::from(at_u32(view.extra, at + 2).cast_signed());
                            if fits_mem_off(size, off) {
                                let rval = if is_fp {
                                    load_fp(&mut words, val, V30)
                                } else {
                                    load_gpr(&mut words, val, X16)
                                };
                                let rbase = load_gpr(&mut words, ptr, X17);
                                emit_mem_imm(&mut words, false, is_fp, size, rval, rbase, off);
                            } else {
                                let rptr = load_gpr(&mut words, ptr, X16);
                                emit_const_u64(&mut words, X17, off.cast_unsigned());
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
                Op::Memcpy => {
                    // 64 B/iter NEON `ldp q`/`stp q` + overlapping head/tail for const `<= 256 B`,
                    // else C runtime `memcpy(dst, src, len)`!
                    let at = to_idx(at_u32(view.data, inst));
                    let dst_v = at_u32(view.extra, at);
                    let src_v = at_u32(view.extra, at + 1);
                    let len_v = at_u32(view.extra, at + 2);
                    let Some(n) = at_opt(&const_u64, to_idx(len_v)).filter(|&n| n <= SOTA_BULK_INLINE_MOST) else {
                        gpr_moves.clear();
                        const_edge_writes.clear();
                        for (k, arg_v) in [(X0, dst_v), (X1, src_v), (X2, len_v)] {
                            let dst = GprCopyLoc::Reg(k);
                            match at_loc(&reg_alloc.loc_of, to_idx(arg_v)) {
                                AllocLoc::Gpr(s) => gpr_moves.push((dst, GprCopyLoc::Reg(u32::from(s)))),
                                AllocLoc::GprSpill(s) => gpr_moves.push((dst, GprCopyLoc::Spill(s))),
                                AllocLoc::RematConst(cval) => const_edge_writes.push((dst, cval)),
                                AllocLoc::None => {
                                    let cval = at_opt(&const_u64, to_idx(arg_v)).ok_or(TierError::Refused {
                                        tier: TIER,
                                        inst: Some(u32_of(inst)),
                                        what: "unallocated memcpy arg",
                                    })?;
                                    const_edge_writes.push((dst, cval));
                                }
                                _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unexpected memcpy arg loc" }),
                            }
                        }
                        emit_parallel_gpr_moves(&mut words, &mut gpr_moves, spill_base)?;
                        for &(dst, cval) in const_edge_writes.iter() {
                            if let GprCopyLoc::Reg(r) = dst {
                                emit_const_u64(&mut words, r, cval);
                            }
                        }
                        let target = resolve_c_routine("memcpy", &mut out);
                        emit_routine_call(&mut words, target, call_mode, &mut out);
                        inst += 1;
                        continue;
                    };
                    let orig_len = to_idx(n);
                    let mut rem = orig_len;
                    if orig_len > 0 {
                        let rdst = load_gpr(&mut words, dst_v, X16);
                        let rsrc = load_gpr(&mut words, src_v, X17);
                        let mut off = 0usize;
                        // 64 B/iter unrolled NEON pair transfers (`V29`/`V30` are reserved scratch).
                        while rem >= 64 && (off + 32) / 16 <= 63 {
                            words.push(enc::ldp_q_off(V29, V30, rsrc, i32_of(off / 16)));
                            words.push(enc::stp_q_off(V29, V30, rdst, i32_of(off / 16)));
                            words.push(enc::ldp_q_off(V29, V30, rsrc, i32_of((off + 32) / 16)));
                            words.push(enc::stp_q_off(V29, V30, rdst, i32_of((off + 32) / 16)));
                            off += 64;
                            rem -= 64;
                        }
                        while rem >= 32 && off / 16 <= 63 {
                            words.push(enc::ldp_q_off(V29, V30, rsrc, i32_of(off / 16)));
                            words.push(enc::stp_q_off(V29, V30, rdst, i32_of(off / 16)));
                            off += 32;
                            rem -= 32;
                        }
                        while rem >= 16 {
                            words.push(enc::ldr_q(V30, rsrc, u32_of(off / 16)));
                            words.push(enc::str_q(V30, rdst, u32_of(off / 16)));
                            off += 16;
                            rem -= 16;
                        }
                        if rem > 0 {
                            if orig_len >= 16 && (orig_len - 16) <= 255 {
                                // Overlapping 16-byte NEON tail: 1 load + 1 store covers ANY 1..=15B tail!
                                let tail_off = i32_of(orig_len - 16);
                                words.push(enc::ldur_q(V30, rsrc, tail_off));
                                words.push(enc::stur_q(V30, rdst, tail_off));
                            } else if orig_len >= 9 && orig_len <= 15 {
                                // Overlapping 8-byte head + tail: 2 loads + 2 stores!
                                let tail_off = i32_of(orig_len - 8);
                                words.push(enc::fp_access(enc::ldr(Size::X, V29, rsrc, 0)));
                                words.push(enc::fp_access(enc::ldur(Size::X, V30, rsrc, tail_off)));
                                words.push(enc::fp_access(enc::str(Size::X, V29, rdst, 0)));
                                words.push(enc::fp_access(enc::stur(Size::X, V30, rdst, tail_off)));
                            } else if orig_len >= 5 && orig_len <= 7 {
                                // Overlapping 4-byte head + tail: 2 loads + 2 stores!
                                let tail_off = i32_of(orig_len - 4);
                                words.push(enc::fp_access(enc::ldr(Size::W, V29, rsrc, 0)));
                                words.push(enc::fp_access(enc::ldur(Size::W, V30, rsrc, tail_off)));
                                words.push(enc::fp_access(enc::str(Size::W, V29, rdst, 0)));
                                words.push(enc::fp_access(enc::stur(Size::W, V30, rdst, tail_off)));
                            } else if orig_len == 3 {
                                // Overlapping 2-byte head + tail: 2 loads + 2 stores!
                                words.push(enc::fp_access(enc::ldr(Size::H, V29, rsrc, 0)));
                                words.push(enc::fp_access(enc::ldur(Size::H, V30, rsrc, 1)));
                                words.push(enc::fp_access(enc::str(Size::H, V29, rdst, 0)));
                                words.push(enc::fp_access(enc::stur(Size::H, V30, rdst, 1)));
                            } else {
                                while rem >= 8 {
                                    words.push(enc::fp_access(enc::ldr(Size::X, V30, rsrc, u32_of(off / 8))));
                                    words.push(enc::fp_access(enc::str(Size::X, V30, rdst, u32_of(off / 8))));
                                    off += 8;
                                    rem -= 8;
                                }
                                if rem >= 4 {
                                    words.push(enc::fp_access(enc::ldr(Size::W, V30, rsrc, u32_of(off / 4))));
                                    words.push(enc::fp_access(enc::str(Size::W, V30, rdst, u32_of(off / 4))));
                                    off += 4;
                                    rem -= 4;
                                }
                                if rem >= 2 {
                                    words.push(enc::fp_access(enc::ldr(Size::H, V30, rsrc, u32_of(off / 2))));
                                    words.push(enc::fp_access(enc::str(Size::H, V30, rdst, u32_of(off / 2))));
                                    off += 2;
                                    rem -= 2;
                                }
                                if rem == 1 {
                                    words.push(enc::fp_access(enc::ldr(Size::B, V30, rsrc, u32_of(off))));
                                    words.push(enc::fp_access(enc::str(Size::B, V30, rdst, u32_of(off))));
                                }
                            }
                        }
                    }
                }
                Op::Memmove => {
                    let at = to_idx(at_u32(view.data, inst));
                    let dst_v = at_u32(view.extra, at);
                    let src_v = at_u32(view.extra, at + 1);
                    let len_v = at_u32(view.extra, at + 2);
                    gpr_moves.clear();
                    const_edge_writes.clear();
                    for (k, arg_v) in [(X0, dst_v), (X1, src_v), (X2, len_v)] {
                        let dst = GprCopyLoc::Reg(k);
                        match at_loc(&reg_alloc.loc_of, to_idx(arg_v)) {
                            AllocLoc::Gpr(s) => gpr_moves.push((dst, GprCopyLoc::Reg(u32::from(s)))),
                            AllocLoc::GprSpill(s) => gpr_moves.push((dst, GprCopyLoc::Spill(s))),
                            AllocLoc::RematConst(cval) => const_edge_writes.push((dst, cval)),
                            AllocLoc::None => {
                                let cval = at_opt(&const_u64, to_idx(arg_v)).ok_or(TierError::Refused {
                                    tier: TIER,
                                    inst: Some(u32_of(inst)),
                                    what: "unallocated memmove arg",
                                })?;
                                const_edge_writes.push((dst, cval));
                            }
                            _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unexpected memmove arg loc" }),
                        }
                    }
                    emit_parallel_gpr_moves(&mut words, &mut gpr_moves, spill_base)?;
                    for &(dst, cval) in const_edge_writes.iter() {
                        if let GprCopyLoc::Reg(r) = dst {
                            emit_const_u64(&mut words, r, cval);
                        }
                    }
                    let target = resolve_c_routine("memmove", &mut out);
                    emit_routine_call(&mut words, target, call_mode, &mut out);
                }
                Op::Memset => {
                    // 32 B/instr NEON `stp q30, q30` + overlapping head/tail for const `<= 256 B`,
                    // else C runtime `memset(dst, byte, len)`!
                    let at = to_idx(at_u32(view.data, inst));
                    let dst_v = at_u32(view.extra, at);
                    let byte_v = at_u32(view.extra, at + 1);
                    let len_v = at_u32(view.extra, at + 2);
                    let Some(n) = at_opt(&const_u64, to_idx(len_v)).filter(|&n| n <= SOTA_BULK_INLINE_MOST) else {
                        gpr_moves.clear();
                        const_edge_writes.clear();
                        for (k, arg_v) in [(X0, dst_v), (X1, byte_v), (X2, len_v)] {
                            let dst = GprCopyLoc::Reg(k);
                            match at_loc(&reg_alloc.loc_of, to_idx(arg_v)) {
                                AllocLoc::Gpr(s) => gpr_moves.push((dst, GprCopyLoc::Reg(u32::from(s)))),
                                AllocLoc::GprSpill(s) => gpr_moves.push((dst, GprCopyLoc::Spill(s))),
                                AllocLoc::RematConst(cval) => const_edge_writes.push((dst, cval)),
                                AllocLoc::None => {
                                    let cval = at_opt(&const_u64, to_idx(arg_v)).ok_or(TierError::Refused {
                                        tier: TIER,
                                        inst: Some(u32_of(inst)),
                                        what: "unallocated memset arg",
                                    })?;
                                    const_edge_writes.push((dst, cval));
                                }
                                _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unexpected memset arg loc" }),
                            }
                        }
                        emit_parallel_gpr_moves(&mut words, &mut gpr_moves, spill_base)?;
                        for &(dst, cval) in const_edge_writes.iter() {
                            if let GprCopyLoc::Reg(r) = dst {
                                emit_const_u64(&mut words, r, cval);
                            }
                        }
                        let target = resolve_c_routine("memset", &mut out);
                        emit_routine_call(&mut words, target, call_mode, &mut out);
                        inst += 1;
                        continue;
                    };
                    let orig_len = to_idx(n);
                    let mut rem = orig_len;
                    if orig_len > 0 {
                        let rdst = load_gpr(&mut words, dst_v, X16);
                        let is_zero = at_opt(&const_u64, to_idx(byte_v)) == Some(0);
                        if orig_len >= 16 {
                            if is_zero {
                                words.push(enc::movi_zero_v(V30));
                            } else {
                                let rb = load_gpr(&mut words, byte_v, X17);
                                words.push(enc::dup_gen_v(0, V30, rb));
                            }
                            let mut off = 0usize;
                            while rem >= 32 && off / 16 <= 63 {
                                words.push(enc::stp_q_off(V30, V30, rdst, i32_of(off / 16)));
                                off += 32;
                                rem -= 32;
                            }
                            while rem >= 16 {
                                words.push(enc::str_q(V30, rdst, u32_of(off / 16)));
                                off += 16;
                                rem -= 16;
                            }
                            if rem > 0 {
                                if (orig_len - 16) <= 255 {
                                    words.push(enc::stur_q(V30, rdst, i32_of(orig_len - 16)));
                                } else {
                                    while rem >= 8 {
                                        words.push(enc::fp_access(enc::str(Size::X, V30, rdst, u32_of(off / 8))));
                                        off += 8;
                                        rem -= 8;
                                    }
                                    if rem >= 4 {
                                        words.push(enc::fp_access(enc::str(Size::W, V30, rdst, u32_of(off / 4))));
                                        off += 4;
                                        rem -= 4;
                                    }
                                    if rem >= 2 {
                                        words.push(enc::fp_access(enc::str(Size::H, V30, rdst, u32_of(off / 2))));
                                        off += 2;
                                        rem -= 2;
                                    }
                                    if rem == 1 {
                                        words.push(enc::fp_access(enc::str(Size::B, V30, rdst, u32_of(off))));
                                    }
                                }
                            }
                        } else {
                            let fill_reg = if is_zero {
                                ZR
                            } else {
                                let rb = load_gpr(&mut words, byte_v, X17);
                                words.push(enc::ubfm(X17, rb, 0, 7));
                                words.push(enc::add_lsl(X17, X17, X17, 8));
                                words.push(enc::add_lsl(X17, X17, X17, 16));
                                words.push(enc::add_lsl(X17, X17, X17, 32));
                                X17
                            };
                            if orig_len >= 9 {
                                words.push(enc::str(Size::X, fill_reg, rdst, 0));
                                words.push(enc::stur(Size::X, fill_reg, rdst, i32_of(orig_len - 8)));
                            } else if orig_len == 8 {
                                words.push(enc::str(Size::X, fill_reg, rdst, 0));
                            } else if orig_len >= 5 {
                                words.push(enc::str(Size::W, fill_reg, rdst, 0));
                                words.push(enc::stur(Size::W, fill_reg, rdst, i32_of(orig_len - 4)));
                            } else if orig_len == 4 {
                                words.push(enc::str(Size::W, fill_reg, rdst, 0));
                            } else if orig_len == 3 {
                                words.push(enc::str(Size::H, fill_reg, rdst, 0));
                                words.push(enc::stur(Size::H, fill_reg, rdst, 1));
                            } else if orig_len == 2 {
                                words.push(enc::str(Size::H, fill_reg, rdst, 0));
                            } else {
                                words.push(enc::str(Size::B, fill_reg, rdst, 0));
                            }
                        }
                    }
                }
                Op::Memcmp => {
                    // 16 B/iter NEON `cmeq.16b + uminv` for const `<= 256 B`, else C runtime `memcmp(a, b, len)`!
                    let at = to_idx(at_u32(view.data, inst));
                    let a_v = at_u32(view.extra, at);
                    let b_v = at_u32(view.extra, at + 1);
                    let len_v = at_u32(view.extra, at + 2);
                    let (rd, sp) = dst_gpr(inst);
                    let Some(n) = at_opt(&const_u64, to_idx(len_v)).filter(|&n| n <= SOTA_BULK_INLINE_MOST) else {
                        gpr_moves.clear();
                        const_edge_writes.clear();
                        for (k, arg_v) in [(X0, a_v), (X1, b_v), (X2, len_v)] {
                            let dst = GprCopyLoc::Reg(k);
                            match at_loc(&reg_alloc.loc_of, to_idx(arg_v)) {
                                AllocLoc::Gpr(s) => gpr_moves.push((dst, GprCopyLoc::Reg(u32::from(s)))),
                                AllocLoc::GprSpill(s) => gpr_moves.push((dst, GprCopyLoc::Spill(s))),
                                AllocLoc::RematConst(cval) => const_edge_writes.push((dst, cval)),
                                AllocLoc::None => {
                                    let cval = at_opt(&const_u64, to_idx(arg_v)).ok_or(TierError::Refused {
                                        tier: TIER,
                                        inst: Some(u32_of(inst)),
                                        what: "unallocated memcmp arg",
                                    })?;
                                    const_edge_writes.push((dst, cval));
                                }
                                _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unexpected memcmp arg loc" }),
                            }
                        }
                        emit_parallel_gpr_moves(&mut words, &mut gpr_moves, spill_base)?;
                        for &(dst, cval) in const_edge_writes.iter() {
                            if let GprCopyLoc::Reg(r) = dst {
                                emit_const_u64(&mut words, r, cval);
                            }
                        }
                        let target = resolve_c_routine("memcmp", &mut out);
                        emit_routine_call(&mut words, target, call_mode, &mut out);
                        if rd != X0 {
                            words.push(enc::mov(rd, X0));
                        }
                        emit_narrow(&mut words, rd, 32);
                        if let Some(s) = sp {
                            emit_st_spill_gpr(&mut words, rd, spill_base, s);
                        }
                        inst += 1;
                        continue;
                    };
                    let orig_len = to_idx(n);
                    if orig_len == 0 {
                        words.push(enc::movz(rd, 0, 0));
                    } else if orig_len >= 16 {
                        let a_in_gpr = matches!(at_loc(&reg_alloc.loc_of, to_idx(a_v)), AllocLoc::Gpr(_));
                        let b_in_gpr = matches!(at_loc(&reg_alloc.loc_of, to_idx(b_v)), AllocLoc::Gpr(_));
                        let ra_fixed = if a_in_gpr { load_gpr(&mut words, a_v, X16) } else { X16 };
                        let rb_fixed = if b_in_gpr { load_gpr(&mut words, b_v, X17) } else { X17 };
                        let mut mismatch_brs: Vec<usize, Alloc<'a>> = Vec::with_capacity_in((orig_len / 16) + 2, alloc);
                        let mut off = 0usize;
                        while off + 16 <= orig_len {
                            let ra = if a_in_gpr { ra_fixed } else { load_gpr(&mut words, a_v, X16) };
                            let rb = if b_in_gpr { rb_fixed } else { load_gpr(&mut words, b_v, X17) };
                            words.push(enc::ldr_q(V29, ra, u32_of(off / 16)));
                            words.push(enc::ldr_q(V30, rb, u32_of(off / 16)));
                            words.push(enc::cmeq_v(true, 0, V31, V29, V30));
                            words.push(enc::uminv_b(V31, V31));
                            words.push(enc::fmov_from_s(X16, V31));
                            let w_br = words.len();
                            words.push(enc::cbz(X16, 0));
                            mismatch_brs.push(w_br);
                            off += 16;
                        }
                        if off < orig_len {
                            // Overlapping 16-byte tail `[orig_len - 16 .. orig_len]`: all bytes `< off` already matched!
                            let tail_off = i32_of(orig_len - 16);
                            let ra = if a_in_gpr { ra_fixed } else { load_gpr(&mut words, a_v, X16) };
                            let rb = if b_in_gpr { rb_fixed } else { load_gpr(&mut words, b_v, X17) };
                            words.push(enc::ldur_q(V29, ra, tail_off));
                            words.push(enc::ldur_q(V30, rb, tail_off));
                            words.push(enc::cmeq_v(true, 0, V31, V29, V30));
                            words.push(enc::uminv_b(V31, V31));
                            words.push(enc::fmov_from_s(X16, V31));
                            let w_br = words.len();
                            words.push(enc::cbz(X16, 0));
                            mismatch_brs.push(w_br);
                        }
                        // All bytes matched!
                        words.push(enc::movz(rd, 0, 0));
                        let w_done = words.len();
                        words.push(enc::b(0));

                        // Single shared branchless mismatch handler: `V29`/`V30` already hold the differing 16B!
                        let mismatch_word = words.len();
                        for &w_br in mismatch_brs.iter() {
                            set_at(&mut words, w_br, enc::cbz(X16, i32_of(mismatch_word - w_br)));
                        }
                        words.push(enc::cmeq_v(true, 0, V31, V29, V30));
                        words.push(enc::shrn(8, 4, V31, V31));
                        words.push(enc::fmov_from_d(X16, V31));
                        words.push(enc::orn(X16, ZR, X16));
                        words.push(enc::rbit(X16, X16));
                        words.push(enc::clz(X16, X16));
                        words.push(enc::lsr_imm(X16, X16, 2));
                        words.push(enc::ins_d_gpr(V31, 0, X16));
                        words.push(enc::tbl1_v(V29, V29, V31));
                        words.push(enc::tbl1_v(V30, V30, V31));
                        words.push(enc::fmov_from_s(X16, V29));
                        words.push(enc::ubfm(X16, X16, 0, 7));
                        words.push(enc::fmov_from_s(X17, V30));
                        words.push(enc::ubfm(X17, X17, 0, 7));
                        words.push(enc::sub(rd, X16, X17));
                        emit_narrow(&mut words, rd, 32);

                        let done_word = words.len();
                        set_at(&mut words, w_done, enc::b(i32_of(done_word - w_done)));
                    } else {
                        let a_in_gpr = matches!(at_loc(&reg_alloc.loc_of, to_idx(a_v)), AllocLoc::Gpr(_));
                        let b_in_gpr = matches!(at_loc(&reg_alloc.loc_of, to_idx(b_v)), AllocLoc::Gpr(_));
                        let ra_fixed = if a_in_gpr { load_gpr(&mut words, a_v, X16) } else { X16 };
                        let rb_fixed = if b_in_gpr { load_gpr(&mut words, b_v, X17) } else { X17 };
                        let mut diff_brs: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(orig_len, alloc);
                        for k in 0..orig_len {
                            let ra = if a_in_gpr { ra_fixed } else { load_gpr(&mut words, a_v, X16) };
                            let rb = if b_in_gpr { rb_fixed } else { load_gpr(&mut words, b_v, X17) };
                            words.push(enc::ldr(Size::B, X16, ra, u32_of(k)));
                            words.push(enc::ldr(Size::B, X17, rb, u32_of(k)));
                            words.push(enc::sub(X16, X16, X17));
                            let w_br = words.len();
                            words.push(enc::cbnz(X16, 0));
                            diff_brs.push(w_br);
                        }
                        words.push(enc::movz(rd, 0, 0));
                        let w_done = words.len();
                        words.push(enc::b(0));

                        let diff_word = words.len();
                        for &w_br in diff_brs.iter() {
                            set_at(&mut words, w_br, enc::cbnz(X16, i32_of(diff_word - w_br)));
                        }
                        if rd != X16 {
                            words.push(enc::mov(rd, X16));
                        }
                        emit_narrow(&mut words, rd, 32);
                        let done_word = words.len();
                        set_at(&mut words, w_done, enc::b(i32_of(done_word - w_done)));
                    }
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::TargetIntrinsic => {
                    let at = to_idx(at_u32(view.data, inst));
                    let name = view.intrinsic_name(IntrinsicId(at_u32(view.extra, at)));
                    if name == b"blaze.prefetch.r1" {
                        let ptr_v = at_u32(view.extra, at + 2);
                        let rptr = load_gpr(&mut words, ptr_v, X16);
                        words.push(enc::prfm_pldl1keep(rptr, 0));
                    }
                }
                Op::AtomicLoad => {
                    let ptr = at_u32(view.data, inst);
                    let ord = Flags(at_u8(view.flags, inst)).ordering();
                    let (size, _) = mem_size_of(ty).ok_or(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unsupported mem size" })?;
                    let (rd, sp) = dst_gpr(inst);
                    let rptr = load_gpr(&mut words, ptr, X16);
                    match ord {
                        Some(Ordering::Relaxed) | None => words.push(enc::ldr(size, rd, rptr, 0)),
                        Some(_) => words.push(enc::ldar(size, rd, rptr)),
                    }
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::AtomicStore => {
                    let at = to_idx(at_u32(view.data, inst));
                    let val = at_u32(view.extra, at);
                    let ptr = at_u32(view.extra, at + 1);
                    let ord = Flags(at_u8(view.flags, inst)).ordering();
                    let (size, _) = mem_size_of(ty_at(view.tys, to_idx(val))).ok_or(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unsupported mem size" })?;
                    let rval = load_gpr(&mut words, val, X16);
                    let rptr = load_gpr(&mut words, ptr, X17);
                    match ord {
                        Some(Ordering::Relaxed) | None => words.push(enc::str(size, rval, rptr, 0)),
                        Some(_) => words.push(enc::stlr(size, rval, rptr)),
                    }
                }
                Op::AtomicRmw => {
                    // Single-instruction FEAT_LSE atomic RMW!
                    let at = to_idx(at_u32(view.data, inst));
                    let kind = AtomicRmw::from_word(at_u32(view.extra, at)).ok_or(TierError::Refused {
                        tier: TIER,
                        inst: Some(u32_of(inst)),
                        what: "invalid atomic_rmw kind",
                    })?;
                    let ptr = at_u32(view.extra, at + 1);
                    let val = at_u32(view.extra, at + 2);
                    let ord = Flags(at_u8(view.flags, inst)).ordering().unwrap_or(Ordering::SeqCst);
                    let acq = matches!(ord, Ordering::Acquire | Ordering::AcqRel | Ordering::SeqCst);
                    let rel = matches!(ord, Ordering::Release | Ordering::AcqRel | Ordering::SeqCst);
                    let (size, _) = mem_size_of(ty).ok_or(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unsupported mem size" })?;
                    let (rd, sp) = dst_gpr(inst);
                    let rptr = load_gpr(&mut words, ptr, X16);
                    let mut rval = load_gpr(&mut words, val, X17);
                    let lse_op = match kind {
                        AtomicRmw::Xchg => Lse::Swp,
                        AtomicRmw::Add => Lse::Add,
                        AtomicRmw::Sub => {
                            words.push(enc::neg(X17, rval));
                            rval = X17;
                            Lse::Add
                        }
                        AtomicRmw::And => {
                            words.push(enc::mvn(X17, rval));
                            rval = X17;
                            Lse::Clr
                        }
                        AtomicRmw::Or => Lse::Set,
                        AtomicRmw::Xor => Lse::Eor,
                        AtomicRmw::Max => Lse::Smax,
                        AtomicRmw::Min => Lse::Smin,
                        AtomicRmw::Umax => Lse::Umax,
                        AtomicRmw::Umin => Lse::Umin,
                        AtomicRmw::Nand => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unsupported atomic_rmw nand" }),
                    };
                    words.push(enc::lse(lse_op, size, acq, rel, rval, rd, rptr));
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Cmpxchg => {
                    // Single-instruction FEAT_LSE `cas`/`casa`/`casl`/`casal` returning `{old, ok}`!
                    let at = to_idx(at_u32(view.data, inst));
                    let ptr_v = at_u32(view.extra, at);
                    let exp_v = at_u32(view.extra, at + 1);
                    let new_v = at_u32(view.extra, at + 2);
                    let fl = Flags(at_u8(view.flags, inst));
                    let succ = fl.ordering().unwrap_or(Ordering::SeqCst);
                    let fail = fl.failure().unwrap_or(Ordering::Relaxed);
                    let acq = matches!(succ, Ordering::Acquire | Ordering::AcqRel | Ordering::SeqCst)
                        || matches!(fail, Ordering::Acquire | Ordering::SeqCst);
                    let rel = matches!(succ, Ordering::Release | Ordering::AcqRel | Ordering::SeqCst);
                    let val_ty = ty_at(view.tys, to_idx(exp_v));
                    let (size, _) = mem_size_of(val_ty).ok_or(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unsupported mem size" })?;
                    let vbits = val_ty.bits(64);
                    match at_loc(&reg_alloc.loc_of, inst) {
                        AllocLoc::GprPair(lo, hi) => {
                            let (r_old, r_ok) = (u32::from(lo), u32::from(hi));
                            // `r_old` and `r_ok` are disjoint from all live inputs (`is_rbw == false` for GprPair).
                            let re = load_gpr(&mut words, exp_v, r_old);
                            if re != r_old {
                                words.push(enc::mov(r_old, re));
                            }
                            emit_narrow(&mut words, r_old, vbits);
                            words.push(enc::mov(X16, r_old));
                            let rp = load_gpr(&mut words, ptr_v, r_ok);
                            if rp != r_ok {
                                words.push(enc::mov(r_ok, rp));
                            }
                            let rn = load_gpr(&mut words, new_v, X17);
                            words.push(enc::cas(size, acq, rel, r_old, rn, r_ok));
                            emit_narrow(&mut words, r_old, vbits);
                            words.push(enc::cmp(r_old, X16));
                            words.push(enc::cset(r_ok, Cond::Eq));
                        }
                        AllocLoc::None => {
                            // Dead result, still perform the atomic side effect!
                            let re = load_gpr(&mut words, exp_v, X16);
                            if re != X16 {
                                words.push(enc::mov(X16, re));
                            }
                            emit_narrow(&mut words, X16, vbits);
                            let rn = load_gpr(&mut words, new_v, X17);
                            let rp = match at_loc(&reg_alloc.loc_of, to_idx(ptr_v)) {
                                AllocLoc::Gpr(r) => u32::from(r),
                                _ => {
                                    return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "spilled cmpxchg ptr" });
                                }
                            };
                            words.push(enc::cas(size, acq, rel, X16, rn, rp));
                        }
                        _ => {
                            return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "spilled cmpxchg" });
                        }
                    }
                }
                Op::Fence => {
                    let flags = Flags(at_u8(view.flags, inst));
                    if !flags.contains(Flags::SINGLE_THREAD) {
                        match flags.ordering() {
                            Some(Ordering::Acquire) => words.push(enc::dmb(Barrier::IshLd)),
                            Some(Ordering::Release | Ordering::AcqRel | Ordering::SeqCst) => {
                                words.push(enc::dmb(Barrier::Ish));
                            }
                            _ => {}
                        }
                    }
                }
                Op::Project => {
                    let at = to_idx(at_u32(view.data, inst));
                    let src = to_idx(at_u32(view.extra, at));
                    let field_idx = at_u32(view.extra, at + 1);
                    let (rd, sp) = dst_gpr(inst);
                    match at_loc(&reg_alloc.loc_of, src) {
                        AllocLoc::GprPair(lo, hi) => {
                            let sreg = u32::from(if field_idx == 0 { lo } else { hi });
                            if rd != sreg {
                                words.push(enc::mov(rd, sreg));
                            }
                        }
                        AllocLoc::GprPairSpill(s0, s1) => {
                            let slot = if field_idx == 0 { s0 } else { s1 };
                            emit_ld_spill_gpr(&mut words, rd, spill_base, slot);
                        }
                        _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unexpected project source loc" }),
                    }
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::DataAddr => {
                    let (rd, sp) = dst_gpr(inst);
                    let rec = view.reference(RefId(at_u32(view.data, inst)));
                    match RefKind::from_word(rec.kind) {
                        Some(RefKind::Static) => {
                            let target = resolve_outside(names, Outside::Static(rec.place), Space::Static, true, &mut out, inst)?;
                            let adrp_at = u32_of(words.len() * 4);
                            words.push(enc::adrp(rd, 0));
                            words.push(enc::ldr(Size::X, rd, rd, 0));
                            out.relocs.push(Reloc { at: adrp_at, in_relro: false, kind: RelocKind::GotPage21, target, addend: 0 });
                            out.relocs.push(Reloc { at: adrp_at + 4, in_relro: false, kind: RelocKind::GotPageOff12, target, addend: 0 });
                        }
                        Some(RefKind::Vtable) => {
                            let target = resolve_outside(names, Outside::Vtable(rec.place), Space::Vtable, false, &mut out, inst)?;
                            let adrp_at = u32_of(words.len() * 4);
                            words.push(enc::adrp(rd, 0));
                            words.push(enc::ldr(Size::X, rd, rd, 0));
                            out.relocs.push(Reloc { at: adrp_at, in_relro: false, kind: RelocKind::GotPage21, target, addend: 0 });
                            out.relocs.push(Reloc { at: adrp_at + 4, in_relro: false, kind: RelocKind::GotPageOff12, target, addend: 0 });
                        }
                        Some(RefKind::Data) => {
                            let row = rec.place;
                            let off = place_data_row(row, &view, names, &mut data_relro_off, &mut out, inst, alloc)?;
                            let adrp_at = u32_of(words.len() * 4);
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
                            return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unsupported data_addr" });
                        }
                    }
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::FuncAddr => {
                    let (rd, sp) = dst_gpr(inst);
                    let rec = view.reference(RefId(at_u32(view.data, inst)));
                    let target = resolve_outside(names, Outside::Func(rec.place), Space::Func, true, &mut out, inst)?;
                    let adrp_at = u32_of(words.len() * 4);
                    words.push(enc::adrp(rd, 0));
                    words.push(enc::ldr(Size::X, rd, rd, 0));
                    out.relocs.push(Reloc { at: adrp_at, in_relro: false, kind: RelocKind::GotPage21, target, addend: 0 });
                    out.relocs.push(Reloc { at: adrp_at + 4, in_relro: false, kind: RelocKind::GotPageOff12, target, addend: 0 });
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::TlsAddr => {
                    // Indexed thread-local fast-path + slow-path (`pass/tls_index.rs`):
                    // 13 instructions on the fast path, calls `TlsGetter` only on first thread access.
                    let (rd, sp) = dst_gpr(inst);
                    let rec = view.reference(RefId(at_u32(view.data, inst)));
                    let desc_t = resolve_outside(names, Outside::TlsDesc, Space::Static, false, &mut out, inst)?;
                    let idx_t = resolve_outside(names, Outside::Tls(rec.place), Space::Tls, false, &mut out, inst)?;
                    let getter_t = resolve_outside(names, Outside::TlsGetter, Space::Func, false, &mut out, inst)?;
                    let adrp_desc = u32_of(words.len() * 4);
                    words.push(enc::adrp(X9, 0));
                    words.push(enc::ldr(Size::X, X9, X9, 0));
                    out.relocs.push(Reloc { at: adrp_desc, in_relro: false, kind: RelocKind::GotPage21, target: desc_t, addend: 0 });
                    out.relocs.push(Reloc { at: adrp_desc + 4, in_relro: false, kind: RelocKind::GotPageOff12, target: desc_t, addend: 0 });
                    words.push(enc::ldr(Size::X, X10, X9, DESC_ROOT_WORDS));
                    words.push(enc_tls::and_clear_bit63(X10, X10));
                    if is_macos {
                        words.push(enc::mrs_tpidrro_el0(X11));
                        words.push(enc::and_imm(X11, X11, 1, 61, 60));
                    } else {
                        words.push(enc_tls::mrs_tpidr_el0(X11));
                    }
                    words.push(enc::add(X11, X11, X10));
                    words.push(enc::ldr(Size::X, X12, X11, 0));
                    let adrp_idx = u32_of(words.len() * 4);
                    words.push(enc::adrp(X13, 0));
                    words.push(enc::ldr(Size::X, X13, X13, 0));
                    out.relocs.push(Reloc { at: adrp_idx, in_relro: false, kind: RelocKind::GotPage21, target: idx_t, addend: 0 });
                    out.relocs.push(Reloc { at: adrp_idx + 4, in_relro: false, kind: RelocKind::GotPageOff12, target: idx_t, addend: 0 });
                    let w_no_table = words.len();
                    words.push(enc::cbz(X12, 0));
                    words.push(enc::ldr(Size::X, X14, X12, TABLE_LEN_WORDS));
                    words.push(enc::cmp(X13, X14));
                    let w_too_short = words.len();
                    words.push(enc::b_cond(Cond::Hs, 0));
                    words.push(enc::add_imm(X14, X12, TABLE_SLOTS, false));
                    words.push(enc::ldr_lsl3(X0, X14, X13));
                    let w_found = words.len();
                    words.push(enc::cbnz(X0, 0));
                    let slow_at = words.len();
                    set_at(&mut words, w_no_table, enc::cbz(X12, i32_of(slow_at - w_no_table)));
                    set_at(&mut words, w_too_short, enc::b_cond(Cond::Hs, i32_of(slow_at - w_too_short)));
                    words.push(enc::mov(X0, X9));
                    words.push(enc::mov(X1, X13));
                    words.push(enc::mov(X2, X11));
                    let adrp_get = u32_of(words.len() * 4);
                    words.push(enc::adrp(X16, 0));
                    words.push(enc::ldr(Size::X, X16, X16, 0));
                    out.relocs.push(Reloc { at: adrp_get, in_relro: false, kind: RelocKind::GotPage21, target: getter_t, addend: 0 });
                    out.relocs.push(Reloc { at: adrp_get + 4, in_relro: false, kind: RelocKind::GotPageOff12, target: getter_t, addend: 0 });
                    words.push(enc::blr(X16));
                    let found_at = words.len();
                    set_at(&mut words, w_found, enc::cbnz(X0, i32_of(found_at - w_found)));
                    if rd != X0 {
                        words.push(enc::mov(rd, X0));
                    }
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Neg => {
                    let (rd, sp) = dst_gpr(inst);
                    let src = load_gpr(&mut words, at_u32(view.data, inst), X16);
                    if bits == 32 {
                        words.push(enc::sf(false, enc::neg(rd, src)));
                    } else {
                        words.push(enc::neg(rd, src));
                        emit_narrow(&mut words, rd, bits);
                    }
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Not => {
                    let (rd, sp) = dst_gpr(inst);
                    let src = load_gpr(&mut words, at_u32(view.data, inst), X16);
                    if bits == 32 {
                        words.push(enc::sf(false, enc::mvn(rd, src)));
                    } else {
                        words.push(enc::mvn(rd, src));
                        emit_narrow(&mut words, rd, bits);
                    }
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Add => {
                    let (rd, sp) = dst_gpr(inst);
                    match at_fusion(&fusion, inst) {
                        Fusion::Madd { a, b: mb, acc } => {
                            let spill_cnt = [a, mb, acc]
                                .iter()
                                .filter(|&&v| !matches!(at_loc(&reg_alloc.loc_of, to_idx(v)), AllocLoc::Gpr(_)))
                                .count();
                            if spill_cnt <= 2 {
                                let mut next_s = X16;
                                let mut grab = |words: &mut Vec<u32, Alloc<'a>>, v: u32| -> Reg {
                                    match at_loc(&reg_alloc.loc_of, to_idx(v)) {
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
                        Fusion::AdcsChain { a, b: mb, .. } => {
                            // `U128`/`U256` hardware carry-chain final limb: 1-instruction `adcs rd, ra, rb`!
                            let ra = load_gpr(&mut words, a, X16);
                            let rb = load_gpr(&mut words, mb, X17);
                            words.push(enc::adcs(rd, ra, rb));
                        }
                        _ => {
                            let at = to_idx(at_u32(view.data, inst));
                            let lhs = at_u32(view.extra, at);
                            let rhs = at_u32(view.extra, at + 1);
                            let w32 = bits == 32;
                            if let Some(imm) = at_opt(&const_u64, to_idx(rhs)).filter(|&v| v <= 4095) {
                                let rl = load_gpr(&mut words, lhs, X16);
                                words.push(enc::sf(!w32, enc::add_imm(rd, rl, trunc_u32(imm), false)));
                            } else if let Some(imm) = at_opt(&const_u64, to_idx(lhs)).filter(|&v| v <= 4095) {
                                let rr = load_gpr(&mut words, rhs, X16);
                                words.push(enc::sf(!w32, enc::add_imm(rd, rr, trunc_u32(imm), false)));
                            } else {
                                let rl = load_gpr(&mut words, lhs, X16);
                                let rr = load_gpr(&mut words, rhs, X17);
                                words.push(enc::sf(!w32, enc::add(rd, rl, rr)));
                            }
                            if !w32 {
                                emit_narrow(&mut words, rd, bits);
                            }
                        }
                    }
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Sub => {
                    let (rd, sp) = dst_gpr(inst);
                    match at_fusion(&fusion, inst) {
                        Fusion::Msub { a, b: mb, acc } => {
                            let spill_cnt = [a, mb, acc]
                                .iter()
                                .filter(|&&v| !matches!(at_loc(&reg_alloc.loc_of, to_idx(v)), AllocLoc::Gpr(_)))
                                .count();
                            if spill_cnt <= 2 {
                                let mut next_s = X16;
                                let mut grab = |words: &mut Vec<u32, Alloc<'a>>, v: u32| -> Reg {
                                    match at_loc(&reg_alloc.loc_of, to_idx(v)) {
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
                            let at = to_idx(at_u32(view.data, inst));
                            let lhs = at_u32(view.extra, at);
                            let rhs = at_u32(view.extra, at + 1);
                            let w32 = bits == 32;
                            if let Some(imm) = at_opt(&const_u64, to_idx(rhs)).filter(|&v| v <= 4095) {
                                let rl = load_gpr(&mut words, lhs, X16);
                                words.push(enc::sf(!w32, enc::sub_imm(rd, rl, trunc_u32(imm), false)));
                            } else {
                                let rl = load_gpr(&mut words, lhs, X16);
                                let rr = load_gpr(&mut words, rhs, X17);
                                words.push(enc::sf(!w32, enc::sub(rd, rl, rr)));
                            }
                            if !w32 {
                                emit_narrow(&mut words, rd, bits);
                            }
                        }
                    }
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Mul => {
                    let at = to_idx(at_u32(view.data, inst));
                    let (rd, sp) = dst_gpr(inst);
                    let rl = load_gpr(&mut words, at_u32(view.extra, at), X16);
                    let rr = load_gpr(&mut words, at_u32(view.extra, at + 1), X17);
                    if bits == 32 {
                        words.push(enc::sf(false, enc::mul(rd, rl, rr)));
                    } else {
                        words.push(enc::mul(rd, rl, rr));
                        emit_narrow(&mut words, rd, bits);
                    }
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::And => {
                    let (rd, sp) = dst_gpr(inst);
                    let w32 = bits == 32;
                    if let Fusion::Bic { a, b: nb } = at_fusion(&fusion, inst) {
                        let ra = load_gpr(&mut words, a, X16);
                        let rb = load_gpr(&mut words, nb, X17);
                        words.push(enc::sf(!w32, enc::bic(rd, ra, rb)));
                    } else {
                        let at = to_idx(at_u32(view.data, inst));
                        let lhs = at_u32(view.extra, at);
                        let rhs = at_u32(view.extra, at + 1);
                        if let Some(nrs) = (bits == 64 || w32)
                            .then(|| at_opt(&const_u64, to_idx(rhs)).and_then(|v| enc::logical_imm(v, bits)))
                            .flatten()
                        {
                            let rl = load_gpr(&mut words, lhs, X16);
                            words.push(enc::sf(!w32, enc::and_imm_nrs(rd, rl, u32_of(nrs))));
                        } else if let Some(nrs) = (bits == 64 || w32)
                            .then(|| at_opt(&const_u64, to_idx(lhs)).and_then(|v| enc::logical_imm(v, bits)))
                            .flatten()
                        {
                            let rr = load_gpr(&mut words, rhs, X16);
                            words.push(enc::sf(!w32, enc::and_imm_nrs(rd, rr, u32_of(nrs))));
                        } else if let Some(k) = at_opt(&const_u64, to_idx(rhs)).and_then(is_low_bitmask) {
                            let rl = load_gpr(&mut words, lhs, X16);
                            words.push(enc::ubfm(rd, rl, 0, k - 1));
                        } else if let Some(k) = at_opt(&const_u64, to_idx(lhs)).and_then(is_low_bitmask) {
                            let rr = load_gpr(&mut words, rhs, X16);
                            words.push(enc::ubfm(rd, rr, 0, k - 1));
                        } else {
                            let rl = load_gpr(&mut words, lhs, X16);
                            let rr = load_gpr(&mut words, rhs, X17);
                            words.push(enc::sf(!w32, enc::and(rd, rl, rr)));
                        }
                    }
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Or => {
                    let (rd, sp) = dst_gpr(inst);
                    let w32 = bits == 32;
                    if let Fusion::Orn { a, b: nb } = at_fusion(&fusion, inst) {
                        let ra = load_gpr(&mut words, a, X16);
                        let rb = load_gpr(&mut words, nb, X17);
                        words.push(enc::sf(!w32, enc::orn(rd, ra, rb)));
                    } else {
                        let at = to_idx(at_u32(view.data, inst));
                        let lhs = at_u32(view.extra, at);
                        let rhs = at_u32(view.extra, at + 1);
                        if let Some(nrs) = (bits == 64 || w32)
                            .then(|| at_opt(&const_u64, to_idx(rhs)).and_then(|v| enc::logical_imm(v, bits)))
                            .flatten()
                        {
                            let rl = load_gpr(&mut words, lhs, X16);
                            words.push(enc::sf(!w32, enc::orr_imm_nrs(rd, rl, u32_of(nrs))));
                        } else if let Some(nrs) = (bits == 64 || w32)
                            .then(|| at_opt(&const_u64, to_idx(lhs)).and_then(|v| enc::logical_imm(v, bits)))
                            .flatten()
                        {
                            let rr = load_gpr(&mut words, rhs, X16);
                            words.push(enc::sf(!w32, enc::orr_imm_nrs(rd, rr, u32_of(nrs))));
                        } else {
                            let rl = load_gpr(&mut words, lhs, X16);
                            let rr = load_gpr(&mut words, rhs, X17);
                            words.push(enc::sf(!w32, enc::orr(rd, rl, rr)));
                        }
                    }
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Xor => {
                    let (rd, sp) = dst_gpr(inst);
                    let w32 = bits == 32;
                    match at_fusion(&fusion, inst) {
                        Fusion::Eon { a, b: nb } => {
                            let ra = load_gpr(&mut words, a, X16);
                            let rb = load_gpr(&mut words, nb, X17);
                            words.push(enc::sf(!w32, enc::eon(rd, ra, rb)));
                        }
                        Fusion::EorLsl { a, b: sb, shift, .. } => {
                            let ra = load_gpr(&mut words, a, X16);
                            let rb = load_gpr(&mut words, sb, X17);
                            words.push(enc::eor_lsl(rd, ra, rb, shift));
                        }
                        Fusion::EorLsr { a, b: sb, shift, .. } => {
                            let ra = load_gpr(&mut words, a, X16);
                            let rb = load_gpr(&mut words, sb, X17);
                            words.push(enc::eor_lsr(rd, ra, rb, shift));
                        }
                        _ => {
                            let at = to_idx(at_u32(view.data, inst));
                            let lhs = at_u32(view.extra, at);
                            let rhs = at_u32(view.extra, at + 1);
                            if let Some(nrs) = (bits == 64 || w32)
                                .then(|| at_opt(&const_u64, to_idx(rhs)).and_then(|v| enc::logical_imm(v, bits)))
                                .flatten()
                            {
                                let rl = load_gpr(&mut words, lhs, X16);
                                words.push(enc::sf(!w32, enc::eor_imm_nrs(rd, rl, u32_of(nrs))));
                            } else if let Some(nrs) = (bits == 64 || w32)
                                .then(|| at_opt(&const_u64, to_idx(lhs)).and_then(|v| enc::logical_imm(v, bits)))
                                .flatten()
                            {
                                let rr = load_gpr(&mut words, rhs, X16);
                                words.push(enc::sf(!w32, enc::eor_imm_nrs(rd, rr, u32_of(nrs))));
                            } else {
                                let rl = load_gpr(&mut words, lhs, X16);
                                let rr = load_gpr(&mut words, rhs, X17);
                                words.push(enc::sf(!w32, enc::eor(rd, rl, rr)));
                            }
                        }
                    }
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Shl => {
                    let at = to_idx(at_u32(view.data, inst));
                    let lhs = at_u32(view.extra, at);
                    let rhs = at_u32(view.extra, at + 1);
                    let (rd, sp) = dst_gpr(inst);
                    let rl = load_gpr(&mut words, lhs, X16);
                    if bits == 64 {
                        if let Some(k) = at_opt(&const_u64, to_idx(rhs)) {
                            let sh = trunc_u32(k & 63);
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
                    } else if bits == 32 {
                        if let Some(k) = at_opt(&const_u64, to_idx(rhs)) {
                            let sh = trunc_u32(k & 31);
                            if sh == 0 {
                                words.push(enc::sf(false, enc::mov(rd, rl)));
                            } else {
                                words.push(enc::lsl_imm_w(rd, rl, sh));
                            }
                        } else {
                            let rr = load_gpr(&mut words, rhs, X17);
                            words.push(enc::sf(false, enc::lslv(rd, rl, rr)));
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
                    let at = to_idx(at_u32(view.data, inst));
                    let lhs = at_u32(view.extra, at);
                    let rhs = at_u32(view.extra, at + 1);
                    let (rd, sp) = dst_gpr(inst);
                    let rl = load_gpr(&mut words, lhs, X16);
                    if bits == 64 {
                        if let Some(k) = at_opt(&const_u64, to_idx(rhs)) {
                            let sh = trunc_u32(k & 63);
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
                    } else if bits == 32 {
                        if let Some(k) = at_opt(&const_u64, to_idx(rhs)) {
                            let sh = trunc_u32(k & 31);
                            if sh == 0 {
                                words.push(enc::sf(false, enc::mov(rd, rl)));
                            } else {
                                words.push(enc::lsr_imm_w(rd, rl, sh));
                            }
                        } else {
                            let rr = load_gpr(&mut words, rhs, X17);
                            words.push(enc::sf(false, enc::lsrv(rd, rl, rr)));
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
                    let at = to_idx(at_u32(view.data, inst));
                    let lhs = at_u32(view.extra, at);
                    let rhs = at_u32(view.extra, at + 1);
                    let (rd, sp) = dst_gpr(inst);
                    let rl = load_gpr(&mut words, lhs, X16);
                    if bits == 64 {
                        if let Some(k) = at_opt(&const_u64, to_idx(rhs)) {
                            let sh = trunc_u32(k & 63);
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
                    } else if bits == 32 {
                        if let Some(k) = at_opt(&const_u64, to_idx(rhs)) {
                            let sh = trunc_u32(k & 31);
                            if sh == 0 {
                                words.push(enc::sf(false, enc::mov(rd, rl)));
                            } else {
                                words.push(enc::asr_imm_w(rd, rl, sh));
                            }
                        } else {
                            let rr = load_gpr(&mut words, rhs, X17);
                            words.push(enc::sf(false, enc::asrv(rd, rl, rr)));
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
                Op::Rotl | Op::Rotr => {
                    let at = to_idx(at_u32(view.data, inst));
                    let lhs = at_u32(view.extra, at);
                    let rhs = at_u32(view.extra, at + 1);
                    let (rd, sp) = dst_gpr(inst);
                    let rl = load_gpr(&mut words, lhs, X16);
                    match bits {
                        1 => {
                            if rd != rl {
                                words.push(enc::mov(rd, rl));
                            }
                        }
                        64 => {
                            if let Some(k) = at_opt(&const_u64, to_idx(rhs)) {
                                let sh = trunc_u32(k & 63);
                                let ror_amt = if op == Op::Rotl { (64 - sh) & 63 } else { sh };
                                if ror_amt == 0 {
                                    if rd != rl {
                                        words.push(enc::mov(rd, rl));
                                    }
                                } else {
                                    words.push(enc::extr(rd, rl, rl, ror_amt));
                                }
                            } else {
                                let rr = load_gpr(&mut words, rhs, X17);
                                if op == Op::Rotl {
                                    words.push(enc::neg(X17, rr));
                                    words.push(enc::rorv(rd, rl, X17));
                                } else {
                                    words.push(enc::rorv(rd, rl, rr));
                                }
                            }
                        }
                        32 => {
                            if let Some(k) = at_opt(&const_u64, to_idx(rhs)) {
                                let sh = trunc_u32(k & 31);
                                let ror_amt = if op == Op::Rotl { (32 - sh) & 31 } else { sh };
                                if ror_amt == 0 {
                                    words.push(enc::sf(false, enc::mov(rd, rl)));
                                } else {
                                    words.push(enc::extr_w(rd, rl, rl, ror_amt));
                                }
                            } else {
                                let rr = load_gpr(&mut words, rhs, X17);
                                if op == Op::Rotl {
                                    words.push(enc::neg(X17, rr));
                                    words.push(enc::rorv_w(rd, rl, X17));
                                } else {
                                    words.push(enc::rorv_w(rd, rl, rr));
                                }
                            }
                        }
                        _ => {
                            let rr = load_gpr(&mut words, rhs, X17);
                            words.push(enc::zext(X17, rr, bits.trailing_zeros()));
                            words.push(enc::movz(X16, bits, 0));
                            words.push(enc::sub(X16, X16, X17));
                            let rl_copy = if rl == X16 {
                                load_gpr(&mut words, lhs, rd)
                            } else {
                                rl
                            };
                            if op == Op::Rotl {
                                words.push(enc::lslv(X17, rl_copy, X17));
                                words.push(enc::lsrv(X16, rl_copy, X16));
                            } else {
                                words.push(enc::lsrv(X17, rl_copy, X17));
                                words.push(enc::lslv(X16, rl_copy, X16));
                            }
                            words.push(enc::orr(rd, X17, X16));
                            emit_narrow(&mut words, rd, bits);
                        }
                    }
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Umulhi | Op::Smulhi => {
                    let at = to_idx(at_u32(view.data, inst));
                    let (rd, sp) = dst_gpr(inst);
                    let mut rl = load_gpr(&mut words, at_u32(view.extra, at), X16);
                    let mut rr = load_gpr(&mut words, at_u32(view.extra, at + 1), X17);
                    let signed = op == Op::Smulhi;
                    if bits == 64 {
                        words.push(if signed { enc::smulh(rd, rl, rr) } else { enc::umulh(rd, rl, rr) });
                    } else {
                        if signed {
                            words.push(enc::sext(X16, rl, bits));
                            words.push(enc::sext(X17, rr, bits));
                            rl = X16;
                            rr = X17;
                        }
                        words.push(enc::mul(rd, rl, rr));
                        words.push(if signed { enc::asr_imm(rd, rd, bits) } else { enc::lsr_imm(rd, rd, bits) });
                        emit_narrow(&mut words, rd, bits);
                    }
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Umin | Op::Umax | Op::Smin | Op::Smax => {
                    let at = to_idx(at_u32(view.data, inst));
                    let (rd, sp) = dst_gpr(inst);
                    let mut rl = load_gpr(&mut words, at_u32(view.extra, at), X16);
                    let mut rr = load_gpr(&mut words, at_u32(view.extra, at + 1), X17);
                    let signed = matches!(op, Op::Smin | Op::Smax);
                    if signed && bits < 64 {
                        words.push(enc::sext(X16, rl, bits));
                        words.push(enc::sext(X17, rr, bits));
                        rl = X16;
                        rr = X17;
                    }
                    if has_cssc {
                        let is_min = matches!(op, Op::Umin | Op::Smin);
                        words.push(enc::cssc_minmax(!signed, is_min, rd, rl, rr));
                    } else {
                        let cond = match op {
                            Op::Umin => Cond::Lo,
                            Op::Umax => Cond::Hi,
                            Op::Smin => Cond::Lt,
                            _ => Cond::Gt,
                        };
                        words.push(enc::cmp(rl, rr));
                        words.push(enc::csel(rd, rl, rr, cond));
                    }
                    emit_narrow(&mut words, rd, bits);
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::UaddSat | Op::UsubSat | Op::SaddSat | Op::SsubSat => {
                    let at = to_idx(at_u32(view.data, inst));
                    let (rd, sp) = dst_gpr(inst);
                    let mut rl = load_gpr(&mut words, at_u32(view.extra, at), X16);
                    let mut rr = load_gpr(&mut words, at_u32(view.extra, at + 1), X17);
                    let signed = matches!(op, Op::SaddSat | Op::SsubSat);
                    if signed && bits < 64 {
                        words.push(enc::sext(X16, rl, bits));
                        words.push(enc::sext(X17, rr, bits));
                        rl = X16;
                        rr = X17;
                    }
                    match op {
                        Op::UsubSat => {
                            words.push(enc::subs(rd, rl, rr));
                            words.push(enc::csel(rd, ZR, rd, Cond::Lo));
                        }
                        Op::UaddSat if bits == 64 => {
                            words.push(enc::adds(rd, rl, rr));
                            words.push(enc::csinv(rd, rd, ZR, Cond::Lo));
                        }
                        Op::UaddSat => {
                            let max_v = (1u64 << bits) - 1;
                            words.push(enc::add(rd, rl, rr));
                            emit_const_u64(&mut words, X17, max_v);
                            words.push(enc::cmp(rd, X17));
                            words.push(enc::csel(rd, X17, rd, Cond::Hi));
                        }
                        Op::SaddSat | Op::SsubSat if bits == 64 => {
                            words.push(if op == Op::SaddSat { enc::adds(rd, rl, rr) } else { enc::subs(rd, rl, rr) });
                            // On overflow: sign bit of wrapped result is inverted, so `bound = (rd >> 63) ^ (1 << 63)`.
                            words.push(enc::asr_imm(X16, rd, 63));
                            words.push(enc::movz(X17, 0x8000, 3));
                            words.push(enc::eor(X16, X16, X17));
                            words.push(enc::csel(rd, X16, rd, Cond::Vs));
                        }
                        _ => {
                            let max_v = (1u64 << (bits - 1)) - 1;
                            words.push(if op == Op::SaddSat { enc::add(rd, rl, rr) } else { enc::sub(rd, rl, rr) });
                            emit_const_u64(&mut words, X17, max_v);
                            words.push(enc::cmp(rd, X17));
                            words.push(enc::csel(rd, X17, rd, Cond::Gt));
                            emit_const_u64(&mut words, X17, !max_v);
                            words.push(enc::cmp(rd, X17));
                            words.push(enc::csel(rd, X17, rd, Cond::Lt));
                        }
                    }
                    emit_narrow(&mut words, rd, bits);
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::UaddOverflow
                | Op::SaddOverflow
                | Op::UsubOverflow
                | Op::SsubOverflow
                | Op::UmulOverflow
                | Op::SmulOverflow => {
                    if let Fusion::AdcsOverflowChain { a, b: mb, carry_out_or, .. } = at_fusion(&fusion, inst) {
                        // Multi-limb `U256` hardware carry-chain limb: 1-instruction `adcs rd_lo, ra, rb`!
                        // `cset` does not modify NZCV flags, so `C` stays live for the next `adcs` limb!
                        let ra = load_gpr(&mut words, a, X16);
                        let rb = load_gpr(&mut words, mb, X17);
                        let (rd_lo, rd_hi, sp_pair) = match at_loc(&reg_alloc.loc_of, inst) {
                            AllocLoc::GprPair(lo, hi) => (u32::from(lo), u32::from(hi), None),
                            AllocLoc::GprPairSpill(s0, s1) => (X16, X17, Some((s0, s1))),
                            AllocLoc::None => (X16, X17, None),
                            _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unexpected adcs overflow pair loc" }),
                        };
                        words.push(enc::adcs(rd_lo, ra, rb));
                        words.push(enc::cset(rd_hi, Cond::Hs));
                        if let Some((s0, s1)) = sp_pair {
                            emit_st_spill_gpr(&mut words, rd_lo, spill_base, s0);
                            emit_st_spill_gpr(&mut words, rd_hi, spill_base, s1);
                        }
                        if let Some(co) = carry_out_or {
                            match at_loc(&reg_alloc.loc_of, to_idx(co)) {
                                AllocLoc::Gpr(rco) => words.push(enc::cset(u32::from(rco), Cond::Hs)),
                                AllocLoc::GprSpill(s) => {
                                    words.push(enc::cset(X17, Cond::Hs));
                                    emit_st_spill_gpr(&mut words, X17, spill_base, s);
                                }
                                _ => {}
                            }
                        }
                        inst += 1;
                        continue;
                    }
                    let at = to_idx(at_u32(view.data, inst));
                    let a_v = at_u32(view.extra, at);
                    let b_v = at_u32(view.extra, at + 1);
                    let w = ty_at(view.tys, to_idx(a_v)).bits(64);
                    let signed = matches!(op, Op::SaddOverflow | Op::SsubOverflow | Op::SmulOverflow);
                    let mut rl = load_gpr(&mut words, a_v, X16);
                    let mut rr = load_gpr(&mut words, b_v, X17);
                    if signed && w < 64 {
                        words.push(enc::sext(X16, rl, w));
                        words.push(enc::sext(X17, rr, w));
                        rl = X16;
                        rr = X17;
                    }
                    let (rd_lo, rd_hi, sp_pair) = match at_loc(&reg_alloc.loc_of, inst) {
                        AllocLoc::GprPair(lo, hi) => (u32::from(lo), u32::from(hi), None),
                        AllocLoc::GprPairSpill(s0, s1) => (X16, X17, Some((s0, s1))),
                        AllocLoc::None => (X16, X17, None),
                        _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unexpected overflow pair loc" }),
                    };
                    // Be careful not to clobber `rl`/`rr` before reading them for `umulh`/`smulh`.
                    match (op, w) {
                        (Op::UaddOverflow, 64) => {
                            words.push(enc::adds(rd_lo, rl, rr));
                            words.push(enc::cset(rd_hi, Cond::Hs));
                        }
                        (Op::SaddOverflow, 64) => {
                            words.push(enc::adds(rd_lo, rl, rr));
                            words.push(enc::cset(rd_hi, Cond::Vs));
                        }
                        (Op::UsubOverflow, _) => {
                            words.push(enc::subs(rd_lo, rl, rr));
                            words.push(enc::cset(rd_hi, Cond::Lo));
                        }
                        (Op::SsubOverflow, 64) => {
                            words.push(enc::subs(rd_lo, rl, rr));
                            words.push(enc::cset(rd_hi, Cond::Vs));
                        }
                        (Op::UmulOverflow, 64) => {
                            // Compute high half BEFORE overwriting if `rd_lo` aliases `rl`/`rr`!
                            if rd_lo == rl || rd_lo == rr {
                                words.push(enc::umulh(rd_hi, rl, rr));
                                words.push(enc::mul(rd_lo, rl, rr));
                            } else {
                                words.push(enc::mul(rd_lo, rl, rr));
                                words.push(enc::umulh(rd_hi, rl, rr));
                            }
                            words.push(enc::cmp_imm(rd_hi, 0));
                            words.push(enc::cset(rd_hi, Cond::Ne));
                        }
                        (Op::SmulOverflow, 64) => {
                            if rd_lo == rl || rd_lo == rr {
                                words.push(enc::smulh(rd_hi, rl, rr));
                                words.push(enc::mul(rd_lo, rl, rr));
                            } else {
                                words.push(enc::mul(rd_lo, rl, rr));
                                words.push(enc::smulh(rd_hi, rl, rr));
                            }
                            let tmp = if rd_lo == X16 || rd_hi == X16 { X17 } else { X16 };
                            // Wait: if `sp_pair.is_some()` then `rd_lo=X16, rd_hi=X17`; spill `rd_lo` first so X16 is free!
                            if let Some((s0, s1)) = sp_pair {
                                emit_st_spill_gpr(&mut words, rd_lo, spill_base, s0);
                                words.push(enc::asr_imm(X16, rd_lo, 63));
                                words.push(enc::cmp(rd_hi, X16));
                                words.push(enc::cset(rd_hi, Cond::Ne));
                                emit_st_spill_gpr(&mut words, rd_hi, spill_base, s1);
                                inst += 1;
                                continue;
                            } else {
                                words.push(enc::asr_imm(tmp, rd_lo, 63));
                                words.push(enc::cmp(rd_hi, tmp));
                                words.push(enc::cset(rd_hi, Cond::Ne));
                            }
                        }
                        _ => {
                            words.push(match op {
                                Op::UaddOverflow | Op::SaddOverflow => enc::add(rd_lo, rl, rr),
                                Op::SsubOverflow => enc::sub(rd_lo, rl, rr),
                                _ => enc::mul(rd_lo, rl, rr),
                            });
                            if signed {
                                words.push(enc::sext(rd_hi, rd_lo, w));
                            } else {
                                words.push(enc::zext(rd_hi, rd_lo, w));
                            }
                            words.push(enc::cmp(rd_lo, rd_hi));
                            words.push(enc::cset(rd_hi, Cond::Ne));
                        }
                    }
                    emit_narrow(&mut words, rd_lo, w);
                    if let Some((s0, s1)) = sp_pair {
                        emit_st_spill_gpr(&mut words, rd_lo, spill_base, s0);
                        emit_st_spill_gpr(&mut words, rd_hi, spill_base, s1);
                    }
                }
                Op::Clz | Op::Ctz | Op::Popcnt | Op::Bswap | Op::Bitrev => {
                    let (rd, sp) = dst_gpr(inst);
                    let src = load_gpr(&mut words, at_u32(view.data, inst), X16);
                    match op {
                        Op::Clz => {
                            if bits == 32 {
                                words.push(enc::sf(false, enc::clz(rd, src)));
                            } else {
                                words.push(enc::clz(rd, src));
                                if bits < 64 {
                                    words.push(enc::sub_imm(rd, rd, 64 - bits, false));
                                }
                            }
                        }
                        Op::Ctz => {
                            if bits == 64 {
                                words.push(enc::rbit(rd, src));
                                words.push(enc::clz(rd, rd));
                            } else if bits == 32 {
                                words.push(enc::sf(false, enc::rbit(rd, src)));
                                words.push(enc::sf(false, enc::clz(rd, rd)));
                            } else {
                                words.push(enc::orr_bit(rd, src, bits));
                                words.push(enc::rbit(rd, rd));
                                words.push(enc::clz(rd, rd));
                            }
                        }
                        Op::Popcnt => {
                            if has_cssc {
                                words.push(enc::cssc_cnt(rd, src));
                            } else {
                                words.push(enc::fmov_to_d(V30, src));
                                words.push(enc::cnt_8b(V30, V30));
                                words.push(enc::addv_8b(V30, V30));
                                words.push(enc::fmov_from_d(rd, V30));
                            }
                        }
                        Op::Bswap => match bits {
                            1 | 8 => {
                                if rd != src {
                                    words.push(enc::mov(rd, src));
                                }
                            }
                            16 => words.push(enc::rev16_w(rd, src)),
                            32 => words.push(enc::rev_w(rd, src)),
                            _ => words.push(enc::rev(rd, src)),
                        },
                        Op::Bitrev => {
                            if bits == 32 {
                                words.push(enc::sf(false, enc::rbit(rd, src)));
                            } else {
                                words.push(enc::rbit(rd, src));
                                if bits < 64 {
                                    words.push(enc::lsr_imm(rd, rd, 64 - bits));
                                }
                            }
                        }
                        _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unsupported bit op" }),
                    }
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Udiv => {
                    let at = to_idx(at_u32(view.data, inst));
                    let (rd, sp) = dst_gpr(inst);
                    let ra_v = at_u32(view.extra, at);
                    let rb_v = at_u32(view.extra, at + 1);
                    if bits == 64 {
                        if let Some(plan) = at_opt(&const_u64, to_idx(rb_v)).and_then(compute_magic_udiv64) {
                            let ra = load_gpr(&mut words, ra_v, X16);
                            match plan {
                                MagicUdiv64::CompareGe(d) => {
                                    emit_const_u64(&mut words, X17, d);
                                    words.push(enc::cmp(ra, X17));
                                    words.push(enc::cset(rd, Cond::Hs));
                                }
                                MagicUdiv64::MulShift { magic, shift } => {
                                    emit_const_u64(&mut words, X17, magic);
                                    words.push(enc::umulh(rd, ra, X17));
                                    if shift > 0 {
                                        words.push(enc::lsr_imm(rd, rd, shift));
                                    }
                                }
                                MagicUdiv64::MulAddShift { magic, post_shift } => {
                                    // `q = umulh(ra, magic); t = ((ra - q) >> 1) + q; rd = t >> post_shift`.
                                    emit_const_u64(&mut words, X17, magic);
                                    words.push(enc::umulh(X17, ra, X17));
                                    words.push(enc::sub(rd, ra, X17));
                                    words.push(enc::lsr_imm(rd, rd, 1));
                                    words.push(enc::add(rd, rd, X17));
                                    if post_shift > 0 {
                                        words.push(enc::lsr_imm(rd, rd, post_shift));
                                    }
                                }
                            }
                            if let Some(s) = sp {
                                emit_st_spill_gpr(&mut words, rd, spill_base, s);
                            }
                            inst += 1;
                            continue;
                        }
                    }
                    let ra = load_gpr(&mut words, ra_v, X16);
                    let rb = load_gpr(&mut words, rb_v, X17);
                    words.push(enc::cbnz(rb, 2));
                    words.push(enc::brk(BRK_DIV_ZERO));
                    words.push(enc::udiv(rd, ra, rb));
                    emit_narrow(&mut words, rd, bits);
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Urem => {
                    let at = to_idx(at_u32(view.data, inst));
                    let (rd, sp) = dst_gpr(inst);
                    let ra_v = at_u32(view.extra, at);
                    let rb_v = at_u32(view.extra, at + 1);
                    if bits == 64 && rd != X16 && rd != X17 {
                        if let Some(d) = at_opt(&const_u64, to_idx(rb_v)) {
                            // Fast divmod reuse: if an earlier instruction in this block ALREADY computed
                            // `q = udiv(ra_v, d)` and `q` is still live in a GPR, emit a single `msub`!
                            let mut shared_q: Option<Reg> = None;
                            for prev in range.start..inst {
                                if !at_bool(&skip_emit, prev) && view.op(prev) == Op::Udiv {
                                    let pat = to_idx(at_u32(view.data, prev));
                                    if at_u32(view.extra, pat) == ra_v && at_opt(&const_u64, to_idx(at_u32(view.extra, pat + 1))) == Some(d) {
                                        if let AllocLoc::Gpr(qr) = at_loc(&reg_alloc.loc_of, prev) {
                                            let is_cs = (19..=28).contains(&qr);
                                            let clobbered = ((prev + 1)..inst).any(|mid| {
                                                let mop = view.op(mid);
                                                (!is_cs && matches!(mop, Op::Call | Op::CallIndirect))
                                                    || matches!(
                                                        at_loc(&reg_alloc.loc_of, mid),
                                                        AllocLoc::Gpr(r) if r == qr
                                                    )
                                                    || matches!(
                                                        at_loc(&reg_alloc.loc_of, mid),
                                                        AllocLoc::GprPair(r0, r1) if r0 == qr || r1 == qr
                                                    )
                                            });
                                            if !clobbered {
                                                shared_q = Some(u32::from(qr));
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                            if let Some(q_reg) = shared_q {
                                let ra = load_gpr(&mut words, ra_v, X16);
                                emit_const_u64(&mut words, X17, d);
                                words.push(enc::msub(rd, q_reg, X17, ra));
                                if let Some(s) = sp {
                                    emit_st_spill_gpr(&mut words, rd, spill_base, s);
                                }
                                inst += 1;
                                continue;
                            }
                            if let Some(plan) = compute_magic_udiv64(d) {
                                let ra = load_gpr(&mut words, ra_v, X16);
                                // Compute quotient `q` into `X17` (or `rd` if `rd != ra`), then `ra - q * d`!
                                let q_reg = if rd != ra { rd } else { X17 };
                                match plan {
                                    MagicUdiv64::CompareGe(dv) => {
                                        emit_const_u64(&mut words, X17, dv);
                                        words.push(enc::cmp(ra, X17));
                                        words.push(enc::sub(q_reg, ra, X17));
                                        words.push(enc::csel(rd, q_reg, ra, Cond::Hs));
                                        inst += 1;
                                        continue;
                                    }
                                    MagicUdiv64::MulShift { magic, shift } => {
                                        emit_const_u64(&mut words, X17, magic);
                                        words.push(enc::umulh(q_reg, ra, X17));
                                        if shift > 0 {
                                            words.push(enc::lsr_imm(q_reg, q_reg, shift));
                                        }
                                    }
                                    MagicUdiv64::MulAddShift { magic, post_shift } => {
                                        emit_const_u64(&mut words, X17, magic);
                                        words.push(enc::umulh(X17, ra, X17));
                                        if q_reg != ra && q_reg != X17 {
                                            words.push(enc::sub(q_reg, ra, X17));
                                            words.push(enc::lsr_imm(q_reg, q_reg, 1));
                                            words.push(enc::add(q_reg, q_reg, X17));
                                            if post_shift > 0 {
                                                words.push(enc::lsr_imm(q_reg, q_reg, post_shift));
                                            }
                                        } else {
                                            // `ra` is `rd` (a GPR, not X16!): use X16 for `(ra - q) >> 1`.
                                            words.push(enc::sub(X16, ra, X17));
                                            words.push(enc::lsr_imm(X16, X16, 1));
                                            words.push(enc::add(X17, X16, X17));
                                            if post_shift > 0 {
                                                words.push(enc::lsr_imm(X17, X17, post_shift));
                                            }
                                        }
                                    }
                                }
                                // `rd = ra - q_reg * d` using `msub` (need `d` in the free scratch reg!).
                                let d_reg = if q_reg == X17 { X16 } else { X17 };
                                if d_reg == ra {
                                    // `ra` was spilled into X16 and `q_reg` is `rd` (`!= X17`), so `d_reg` is X17!
                                    emit_const_u64(&mut words, X17, d);
                                    words.push(enc::msub(rd, q_reg, X17, ra));
                                } else {
                                    emit_const_u64(&mut words, d_reg, d);
                                    words.push(enc::msub(rd, q_reg, d_reg, ra));
                                }
                                inst += 1;
                                continue;
                            }
                        }
                    }
                    let ra = load_gpr(&mut words, ra_v, X16);
                    let rb = load_gpr(&mut words, rb_v, X17);
                    words.push(enc::cbnz(rb, 2));
                    words.push(enc::brk(BRK_DIV_ZERO));
                    if rd != X16 && rd != X17 {
                        words.push(enc::udiv(rd, ra, rb));
                        words.push(enc::msub(rd, rd, rb, ra));
                    } else if let Some(s) = sp {
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
                    let at = to_idx(at_u32(view.data, inst));
                    let (rd, sp) = dst_gpr(inst);
                    let ra = load_gpr(&mut words, at_u32(view.extra, at), X16);
                    let rb = load_gpr(&mut words, at_u32(view.extra, at + 1), X17);
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
                    set_at(&mut words, skip_ov, enc::b_cond(Cond::Ne, i32_of(after_ov - skip_ov)));
                    words.push(enc::sdiv(rd, a_reg, b_reg));
                    emit_narrow(&mut words, rd, bits);
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Srem => {
                    let at = to_idx(at_u32(view.data, inst));
                    let (rd, sp) = dst_gpr(inst);
                    let ra = load_gpr(&mut words, at_u32(view.extra, at), X16);
                    let rb = load_gpr(&mut words, at_u32(view.extra, at + 1), X17);
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
                    let Some(double) = fp_ty else { return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "non-fp op" }); };
                    let ft = if double { Ftype::D } else { Ftype::S };
                    let (vd, sp) = dst_fp(inst);
                    match at_fusion(&fusion, inst) {
                        Fusion::Fmadd { a, b: mb, acc } => {
                            let va = load_fp(&mut words, a, V29);
                            let vb = load_fp(&mut words, mb, V30);
                            let vc = load_fp(&mut words, acc, V31);
                            words.push(enc::fmadd(ft, vd, va, vb, vc));
                        }
                        _ => {
                            let at = to_idx(at_u32(view.data, inst));
                            let va = load_fp(&mut words, at_u32(view.extra, at), V30);
                            let vb = load_fp(&mut words, at_u32(view.extra, at + 1), V31);
                            words.push(enc::fbin(0b0010, double, vd, va, vb));
                        }
                    }
                    if let Some(s) = sp {
                        emit_st_spill_fp(&mut words, vd, spill_base, s);
                    }
                }
                Op::Fsub => {
                    let Some(double) = fp_ty else { return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "non-fp op" }); };
                    let ft = if double { Ftype::D } else { Ftype::S };
                    let (vd, sp) = dst_fp(inst);
                    match at_fusion(&fusion, inst) {
                        Fusion::Fmsub { a, b: mb, acc } => {
                            let va = load_fp(&mut words, a, V29);
                            let vb = load_fp(&mut words, mb, V30);
                            let vc = load_fp(&mut words, acc, V31);
                            words.push(enc::fmsub(ft, vd, va, vb, vc));
                        }
                        _ => {
                            let at = to_idx(at_u32(view.data, inst));
                            let va = load_fp(&mut words, at_u32(view.extra, at), V30);
                            let vb = load_fp(&mut words, at_u32(view.extra, at + 1), V31);
                            words.push(enc::fbin(0b0011, double, vd, va, vb));
                        }
                    }
                    if let Some(s) = sp {
                        emit_st_spill_fp(&mut words, vd, spill_base, s);
                    }
                }
                Op::Frem => {
                    let Some(double) = fp_ty else { return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "non-fp op" }); };
                    let (vd, sp) = dst_fp(inst);
                    let at = to_idx(at_u32(view.data, inst));
                    let (a_v, b_v) = (at_u32(view.extra, at), at_u32(view.extra, at + 1));
                    fp_moves.clear();
                    for (dst_r, arg_v) in [(0u32, a_v), (1u32, b_v)] {
                        let dst = FpCopyLoc::Reg(dst_r);
                        match at_loc(&reg_alloc.loc_of, to_idx(arg_v)) {
                            AllocLoc::Fp(s) => fp_moves.push((dst, FpCopyLoc::Reg(u32::from(s)))),
                            AllocLoc::FpSpill(s) => fp_moves.push((dst, FpCopyLoc::Spill(s))),
                            _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unexpected frem arg loc" }),
                        }
                    }
                    emit_parallel_fp_moves(&mut words, &mut fp_moves, spill_base)?;
                    let target = resolve_c_routine(if double { "fmod" } else { "fmodf" }, &mut out);
                    emit_routine_call(&mut words, target, call_mode, &mut out);
                    if vd != 0 {
                        words.push(enc::fmov_reg(double, vd, 0));
                    }
                    if let Some(s) = sp {
                        emit_st_spill_fp(&mut words, vd, spill_base, s);
                    }
                }
                Op::Fmul | Op::Fdiv | Op::Fmaximum | Op::Fminimum | Op::FmaxNum | Op::FminNum => {
                    let Some(double) = fp_ty else { return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "non-fp op" }); };
                    let (vd, sp) = dst_fp(inst);
                    let at = to_idx(at_u32(view.data, inst));
                    let va = load_fp(&mut words, at_u32(view.extra, at), V30);
                    let vb = load_fp(&mut words, at_u32(view.extra, at + 1), V31);
                    let opcode = match op {
                        Op::Fmul => 0b0000,
                        Op::Fdiv => 0b0001,
                        Op::Fmaximum => 0b0100,
                        Op::Fminimum => 0b0101,
                        Op::FmaxNum => 0b0110,
                        Op::FminNum => 0b0111,
                        _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unsupported fp binop" }),
                    };
                    words.push(enc::fbin(opcode, double, vd, va, vb));
                    if let Some(s) = sp {
                        emit_st_spill_fp(&mut words, vd, spill_base, s);
                    }
                }
                Op::Fneg => {
                    let Some(double) = fp_ty else { return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "non-fp op" }); };
                    let (vd, sp) = dst_fp(inst);
                    let va = load_fp(&mut words, at_u32(view.data, inst), V30);
                    words.push(enc::fneg(double, vd, va));
                    if let Some(s) = sp {
                        emit_st_spill_fp(&mut words, vd, spill_base, s);
                    }
                }
                Op::Fabs | Op::Sqrt | Op::Nearest | Op::Ceil | Op::Floor | Op::Ftrunc | Op::Fround => {
                    let Some(double) = fp_ty else { return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "non-fp op" }); };
                    let (vd, sp) = dst_fp(inst);
                    let va = load_fp(&mut words, at_u32(view.data, inst), V30);
                    let opcode = match op {
                        Op::Fabs => 0b000001,
                        Op::Sqrt => 0b000011,
                        Op::Nearest => 0b001000,
                        Op::Ceil => 0b001001,
                        Op::Floor => 0b001010,
                        Op::Ftrunc => 0b001011,
                        Op::Fround => 0b001100,
                        _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unsupported fp unary op" }),
                    };
                    words.push(enc::fp1_op(opcode, double, vd, va));
                    if let Some(s) = sp {
                        emit_st_spill_fp(&mut words, vd, spill_base, s);
                    }
                }
                Op::Fma | Op::Fmuladd => {
                    let Some(double) = fp_ty else { return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "non-fp op" }); };
                    let ft = if double { Ftype::D } else { Ftype::S };
                    let (vd, sp) = dst_fp(inst);
                    let at = to_idx(at_u32(view.data, inst));
                    let va = load_fp(&mut words, at_u32(view.extra, at), V29);
                    let vb = load_fp(&mut words, at_u32(view.extra, at + 1), V30);
                    let vc = load_fp(&mut words, at_u32(view.extra, at + 2), V31);
                    words.push(enc::fmadd(ft, vd, va, vb, vc));
                    if let Some(s) = sp {
                        emit_st_spill_fp(&mut words, vd, spill_base, s);
                    }
                }
                Op::Fpext | Op::Fptrunc => {
                    let Some(to_double) = fp_ty else { return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "non-fp op" }); };
                    let (vd, sp) = dst_fp(inst);
                    let va = load_fp(&mut words, at_u32(view.data, inst), V30);
                    words.push(enc::fcvt(to_double, vd, va));
                    if let Some(s) = sp {
                        emit_st_spill_fp(&mut words, vd, spill_base, s);
                    }
                }
                Op::SitoFp | Op::UitoFp => {
                    let Some(double) = fp_ty else { return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "non-fp op" }); };
                    let (vd, sp) = dst_fp(inst);
                    let src_v = at_u32(view.data, inst);
                    let src_bits = ty_at(view.tys, to_idx(src_v)).bits(64);
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
                    let src_v = at_u32(view.data, inst);
                    let double = is_fp_ty(ty_at(view.tys, to_idx(src_v))).ok_or(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "non-fp type" })?;
                    let signed = op == Op::FptoSi;
                    let (rd, sp) = dst_gpr(inst);
                    let va = load_fp(&mut words, src_v, V30);
                    words.push(enc::fcvtz(signed, bits == 64, double, rd, va));
                    if bits < 32 {
                        let max = (1u64 << (bits - u32::from(signed))) - 1;
                        if signed {
                            words.push(enc::sext(rd, rd, 32));
                            emit_const_u64(&mut words, X17, max);
                            words.push(enc::cmp(rd, X17));
                            words.push(enc::csel(rd, X17, rd, Cond::Gt));
                            emit_const_u64(&mut words, X17, max.cast_signed().wrapping_neg().wrapping_sub(1).cast_unsigned());
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
                    let src_v = at_u32(view.data, inst);
                    let src_fp = is_fp_ty(ty_at(view.tys, to_idx(src_v)));
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
                    let at = to_idx(at_u32(view.data, inst));
                    let cond = IntCond::from_word(at_u32(view.extra, at)).ok_or(TierError::Refused {
                        tier: TIER,
                        inst: Some(u32_of(inst)),
                        what: "invalid icmp cond",
                    })?;
                    let a = at_u32(view.extra, at + 1);
                    let bv = at_u32(view.extra, at + 2);
                    let (rd, sp) = dst_gpr(inst);
                    let op_ty = ty_at(view.tys, to_idx(a));
                    if op_ty == Ty::I128 {
                        let load_h = |words: &mut Vec<u32, Alloc<'a>>, v: u32, hi: bool, sc: Reg| -> Reg {
                            match at_loc(&reg_alloc.loc_of, to_idx(v)) {
                                AllocLoc::GprPair(lo, h) => u32::from(if hi { h } else { lo }),
                                AllocLoc::GprPairSpill(s0, s1) => {
                                    emit_ld_spill_gpr(words, sc, spill_base, if hi { s1 } else { s0 });
                                    sc
                                }
                                _ => {
                                    internal_err.set(Some(TierError::Refused { tier: TIER, inst: Some(v), what: "unexpected i128 icmp operand loc" }));
                                    sc
                                }
                            }
                        };
                        match cond {
                            IntCond::Eq | IntCond::Ne => {
                                let a0 = load_h(&mut words, a, false, X16);
                                let b0 = load_h(&mut words, bv, false, X17);
                                words.push(enc::eor(rd, a0, b0));
                                if let Some(s) = sp {
                                    emit_st_spill_gpr(&mut words, rd, spill_base, s);
                                }
                                let a1 = load_h(&mut words, a, true, X16);
                                let b1 = load_h(&mut words, bv, true, X17);
                                words.push(enc::eor(X16, a1, b1));
                                let prev = if let Some(s) = sp {
                                    emit_ld_spill_gpr(&mut words, X17, spill_base, s);
                                    X17
                                } else {
                                    rd
                                };
                                words.push(enc::orr(X16, prev, X16));
                                words.push(enc::cmp_imm(X16, 0));
                                words.push(enc::cset(rd, if cond == IntCond::Eq { Cond::Eq } else { Cond::Ne }));
                            }
                            IntCond::Ult | IntCond::Uge | IntCond::Slt | IntCond::Sge => {
                                let a0 = load_h(&mut words, a, false, X16);
                                let b0 = load_h(&mut words, bv, false, X17);
                                words.push(enc::cmp(a0, b0));
                                let a1 = load_h(&mut words, a, true, X16);
                                let b1 = load_h(&mut words, bv, true, X17);
                                words.push(enc::sbcs(ZR, a1, b1));
                                let c = match cond {
                                    IntCond::Ult => Cond::Lo,
                                    IntCond::Uge => Cond::Hs,
                                    IntCond::Slt => Cond::Lt,
                                    _ => Cond::Ge,
                                };
                                words.push(enc::cset(rd, c));
                            }
                            _ => {
                                let b0 = load_h(&mut words, bv, false, X16);
                                let a0 = load_h(&mut words, a, false, X17);
                                words.push(enc::cmp(b0, a0));
                                let b1 = load_h(&mut words, bv, true, X16);
                                let a1 = load_h(&mut words, a, true, X17);
                                words.push(enc::sbcs(ZR, b1, a1));
                                let c = match cond {
                                    IntCond::Ugt => Cond::Lo,
                                    IntCond::Ule => Cond::Hs,
                                    IntCond::Sgt => Cond::Lt,
                                    _ => Cond::Ge,
                                };
                                words.push(enc::cset(rd, c));
                            }
                        }
                    } else {
                        emit_cmp(&mut words, cond, a, bv, op_ty);
                        words.push(enc::cset(rd, cond_of(cond)));
                    }
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Fcmp => {
                    let at = to_idx(at_u32(view.data, inst));
                    let cond = FloatCond::from_word(at_u32(view.extra, at)).ok_or(TierError::Refused {
                        tier: TIER,
                        inst: Some(u32_of(inst)),
                        what: "invalid fcmp cond",
                    })?;
                    let a = at_u32(view.extra, at + 1);
                    let bv = at_u32(view.extra, at + 2);
                    let double = is_fp_ty(ty_at(view.tys, to_idx(a))).ok_or(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "non-fp type" })?;
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
                            let fc = fcond_of(other).ok_or(TierError::Refused {
                                tier: TIER,
                                inst: Some(u32_of(inst)),
                                what: "unsupported fcmp cond",
                            })?;
                            words.push(enc::cset(rd, fc));
                        }
                    }
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Select if is_vec128_ty(ty).is_none() => {
                    let at = to_idx(at_u32(view.data, inst));
                    let t = at_u32(view.extra, at + 1);
                    let f = at_u32(view.extra, at + 2);
                    match at_fusion(&fusion, inst) {
                        Fusion::CsnegSelect { cond, a, b: bv, op_ty, x, neg_when_false } => {
                            let (rd, sp) = dst_gpr(inst);
                            let is_abs_zero = has_cssc
                                && op_ty == Ty::I64
                                && a == x
                                && at_opt(&const_u64, to_idx(bv)) == Some(0)
                                && ((matches!(cond, IntCond::Sge | IntCond::Sgt) && neg_when_false)
                                    || (matches!(cond, IntCond::Slt | IntCond::Sle) && !neg_when_false));
                            if is_abs_zero {
                                let rx = load_gpr(&mut words, x, X16);
                                words.push(enc::cssc_abs(rd, rx));
                            } else {
                                emit_cmp(&mut words, cond, a, bv, op_ty);
                                let rx = load_gpr(&mut words, x, X16);
                                let c = if neg_when_false { cond_of(cond) } else { invert_cond(cond_of(cond)) };
                                words.push(enc::csneg(rd, rx, rx, c));
                            }
                            if let Some(s) = sp {
                                emit_st_spill_gpr(&mut words, rd, spill_base, s);
                            }
                            inst += 1;
                            continue;
                        }
                        Fusion::CsincSelect { cond, a, b: bv, op_ty, x, inc_when_false } => {
                            let (rd, sp) = dst_gpr(inst);
                            emit_cmp(&mut words, cond, a, bv, op_ty);
                            let rx = load_gpr(&mut words, x, X16);
                            let c = if inc_when_false { cond_of(cond) } else { invert_cond(cond_of(cond)) };
                            words.push(enc::csinc(rd, rx, rx, c));
                            if let Some(s) = sp {
                                emit_st_spill_gpr(&mut words, rd, spill_base, s);
                            }
                            inst += 1;
                            continue;
                        }
                        Fusion::CsinvSelect { cond, a, b: bv, op_ty, x, inv_when_false } => {
                            let (rd, sp) = dst_gpr(inst);
                            emit_cmp(&mut words, cond, a, bv, op_ty);
                            let rx = load_gpr(&mut words, x, X16);
                            let c = if inv_when_false { cond_of(cond) } else { invert_cond(cond_of(cond)) };
                            words.push(enc::csinv(rd, rx, rx, c));
                            if let Some(s) = sp {
                                emit_st_spill_gpr(&mut words, rd, spill_base, s);
                            }
                            inst += 1;
                            continue;
                        }
                        _ => {}
                    }
                    let sel_cond = match at_fusion(&fusion, inst) {
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
                            let rc = load_gpr(&mut words, at_u32(view.extra, at), X16);
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
                    let src = load_gpr(&mut words, at_u32(view.data, inst), X16);
                    if rd != src {
                        words.push(enc::mov(rd, src));
                    }
                    emit_narrow(&mut words, rd, bits);
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Sext => {
                    let src_v = at_u32(view.data, inst);
                    let src_bits = ty_at(view.tys, to_idx(src_v)).bits(64);
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
                    let src_v = at_u32(view.data, inst);
                    let src = if ty_at(view.tys, to_idx(src_v)) == Ty::I128 {
                        match at_loc(&reg_alloc.loc_of, to_idx(src_v)) {
                            AllocLoc::GprPair(lo, _) => u32::from(lo),
                            AllocLoc::GprPairSpill(s0, _) => {
                                emit_ld_spill_gpr(&mut words, X16, spill_base, s0);
                                X16
                            }
                            _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unexpected i128 trunc src loc" }),
                        }
                    } else {
                        load_gpr(&mut words, src_v, X16)
                    };
                    if bits < 64 {
                        words.push(enc::zext(rd, src, bits));
                    } else if rd != src {
                        words.push(enc::mov(rd, src));
                    }
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Call | Op::CallIndirect | Op::Invoke | Op::InvokeIndirect => {
                    let at = to_idx(at_u32(view.data, inst));
                    let is_direct = matches!(op, Op::Call | Op::Invoke);
                    let is_invoke = matches!(op, Op::Invoke | Op::InvokeIndirect);
                    let sig_id = SigId(at_u32(view.extra, at + 1));
                    let sig = view.sig(sig_id);
                    let invoke_blocks = if is_invoke {
                        Some((at_u32(view.extra, at + 2), at_u32(view.extra, at + 3)))
                    } else {
                        None
                    };
                    let argc = if is_invoke {
                        to_idx(at_u32(view.extra, at + 4))
                    } else {
                        to_idx(at_u32(view.extra, at + 2))
                    };
                    let args_base = if is_invoke { at + 5 } else { at + 3 };
                    let is_tail = !is_invoke && at_bool(&tail_calls, inst);

                    let direct_target = if is_direct {
                        let rec = view.reference(RefId(at_u32(view.extra, at)));
                        Some(resolve_outside(names, Outside::Func(rec.place), Space::Func, true, &mut out, inst)?)
                    } else {
                        None
                    };

                    // 1. Copy `ParamKind::Byval` arguments into this function's reserved outgoing byval area
                    //    (`[sp + byval_base + ...]`) BEFORE touching any argument register.
                    let mut byval_cur = 0u64;
                    let mut byval_offs: Vec<(usize, u32), Alloc<'a>> = Vec::new_in(alloc);
                    for (k, &param) in sig.params.iter().take(argc).enumerate() {
                        if ParamKind::from_byte(param.kind) == Some(ParamKind::Byval) {
                            let facts = view.facts(&param).ok_or(TierError::Refused {
                                tier: TIER,
                                inst: Some(u32_of(inst)),
                                what: "byval without size/align",
                            })?;
                            let align = 1u64 << facts.align;
                            byval_cur = byval_cur.next_multiple_of(align);
                            let copy_off = u32_of(byval_base + byval_cur);
                            byval_cur += u64::from(facts.size);
                            let arg_v = at_u32(view.extra, args_base + k);
                            let src_r = load_gpr(&mut words, arg_v, X17);
                            emit_byval_copy(&mut words, copy_off, src_r, facts.size);
                            byval_offs.push((k, copy_off));
                        }
                    }

                    // 2. Store stacked arguments (`[sp, #slot.at]`) and enqueue register arguments
                    //    (`X0..X7`, `X8` for `sret`, `V0..V7`, and `X17` for indirect callee).
                    gpr_moves.clear();
                    fp_moves.clear();
                    const_edge_writes.clear();
                    if !is_direct {
                        let callee_v = at_u32(view.extra, at);
                        match at_loc(&reg_alloc.loc_of, to_idx(callee_v)) {
                            AllocLoc::Gpr(s) => gpr_moves.push((GprCopyLoc::Reg(X17), GprCopyLoc::Reg(u32::from(s)))),
                            AllocLoc::GprSpill(s) => gpr_moves.push((GprCopyLoc::Reg(X17), GprCopyLoc::Spill(s))),
                            _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unexpected indirect callee loc" }),
                        }
                    }
                    let mut gpr_k = 0usize;
                    let mut fp_k = 0usize;
                    let mut call_nsaa = Nsaa::new(os);
                    let mut sext_args: [(Reg, u32); 8] = [(0, 0); 8];
                    let mut sext_n = 0usize;
                    let mut byval_reg_writes: [(Reg, u32); 8] = [(0, 0); 8];
                    let mut byval_reg_n = 0usize;
                    for (k, &param) in sig.params.iter().take(argc).enumerate() {
                        let arg_v = at_u32(view.extra, args_base + k);
                        let arg_ty = ty_at(view.tys, to_idx(arg_v));
                        let byval_copy_off = if ParamKind::from_byte(param.kind) == Some(ParamKind::Byval) {
                            byval_offs.iter().find(|&&(idx, _)| idx == k).map(|&(_, o)| o)
                        } else {
                            None
                        };
                        let placed = place_param(&param, &mut gpr_k, &mut fp_k, &mut call_nsaa).ok_or(TierError::Refused {
                            tier: TIER,
                            inst: Some(u32_of(inst)),
                            what: "unplaceable call arg",
                        })?;
                        match placed {
                            ParamPlace::Sret => {
                                let dst = GprCopyLoc::Reg(X8);
                                match at_loc(&reg_alloc.loc_of, to_idx(arg_v)) {
                                    AllocLoc::Gpr(s) => gpr_moves.push((dst, GprCopyLoc::Reg(u32::from(s)))),
                                    AllocLoc::GprSpill(s) => gpr_moves.push((dst, GprCopyLoc::Spill(s))),
                                    _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unexpected sret call arg loc" }),
                                }
                            }
                            ParamPlace::Fp(dst_reg) => {
                                let dst = FpCopyLoc::Reg(dst_reg);
                                match at_loc(&reg_alloc.loc_of, to_idx(arg_v)) {
                                    AllocLoc::Fp(s) => fp_moves.push((dst, FpCopyLoc::Reg(u32::from(s)))),
                                    AllocLoc::FpSpill(s) => fp_moves.push((dst, FpCopyLoc::Spill(s))),
                                    _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unexpected FP call arg loc" }),
                                }
                            }
                            ParamPlace::GprPair(r0, r1) => {
                                let dst0 = GprCopyLoc::Reg(r0);
                                let dst1 = GprCopyLoc::Reg(r1);
                                match at_loc(&reg_alloc.loc_of, to_idx(arg_v)) {
                                    AllocLoc::GprPair(s0, s1) => {
                                        gpr_moves.push((dst0, GprCopyLoc::Reg(u32::from(s0))));
                                        gpr_moves.push((dst1, GprCopyLoc::Reg(u32::from(s1))));
                                    }
                                    AllocLoc::GprPairSpill(s0, s1) => {
                                        gpr_moves.push((dst0, GprCopyLoc::Spill(s0)));
                                        gpr_moves.push((dst1, GprCopyLoc::Spill(s1)));
                                    }
                                    _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unexpected i128 call arg loc" }),
                                }
                            }
                            ParamPlace::Gpr(dst_reg) => {
                                if let Some(copy_off) = byval_copy_off {
                                    set_at(&mut byval_reg_writes, byval_reg_n, (dst_reg, copy_off));
                                    byval_reg_n += 1;
                                } else {
                                    let dst = GprCopyLoc::Reg(dst_reg);
                                    match at_loc(&reg_alloc.loc_of, to_idx(arg_v)) {
                                        AllocLoc::Gpr(s) => gpr_moves.push((dst, GprCopyLoc::Reg(u32::from(s)))),
                                        AllocLoc::GprSpill(s) => gpr_moves.push((dst, GprCopyLoc::Spill(s))),
                                        AllocLoc::RematConst(cval) => const_edge_writes.push((dst, cval)),
                                        AllocLoc::None => {
                                            let cval = at_opt(&const_u64, to_idx(arg_v)).ok_or(TierError::Refused {
                                                tier: TIER,
                                                inst: Some(u32_of(inst)),
                                                what: "unallocated non-const call arg",
                                            })?;
                                            const_edge_writes.push((dst, cval));
                                        }
                                        _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unexpected GPR call arg loc" }),
                                    }
                                    if Ext::from_byte(param.ext) == Some(Ext::Sext) {
                                        let w = arg_ty.bits(64);
                                        if w < 64 {
                                            set_at(&mut sext_args, sext_n, (dst_reg, w));
                                            sext_n += 1;
                                        }
                                    }
                                }
                            }
                            ParamPlace::Stack(slot) => {
                                if let Some(copy_off) = byval_copy_off {
                                    emit_sp_addr(&mut words, X16, copy_off);
                                    emit_mem_off(&mut words, false, false, Size::X, X16, SP, slot.at);
                                } else if is_fp_ty(arg_ty).is_some() {
                                    let vs = load_fp(&mut words, arg_v, V30);
                                    let sz = size_of_stack_bytes(slot.bytes);
                                    emit_mem_off(&mut words, false, true, sz, vs, SP, slot.at);
                                } else if arg_ty == Ty::I128 {
                                    match at_loc(&reg_alloc.loc_of, to_idx(arg_v)) {
                                        AllocLoc::GprPair(s0, s1) => {
                                            emit_mem_off(&mut words, false, false, Size::X, u32::from(s0), SP, slot.at);
                                            emit_mem_off(&mut words, false, false, Size::X, u32::from(s1), SP, slot.at + 8);
                                        }
                                        AllocLoc::GprPairSpill(s0, s1) => {
                                            emit_ld_spill_gpr(&mut words, X16, spill_base, s0);
                                            emit_mem_off(&mut words, false, false, Size::X, X16, SP, slot.at);
                                            emit_ld_spill_gpr(&mut words, X16, spill_base, s1);
                                            emit_mem_off(&mut words, false, false, Size::X, X16, SP, slot.at + 8);
                                        }
                                        _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unexpected stacked i128 call arg loc" }),
                                    }
                                } else {
                                    let mut rs = load_gpr(&mut words, arg_v, X16);
                                    let w = arg_ty.bits(64);
                                    if Ext::from_byte(param.ext) == Some(Ext::Sext) && w < slot.bytes * 8 {
                                        words.push(enc::sext(X16, rs, w));
                                        rs = X16;
                                    }
                                    let sz = size_of_stack_bytes(slot.bytes);
                                    emit_mem_off(&mut words, false, false, sz, rs, SP, slot.at);
                                }
                            }
                        }
                    }
                    emit_parallel_moves(&mut words, &mut gpr_moves, spill_base)?;
                    emit_parallel_moves(&mut words, &mut fp_moves, spill_base)?;
                    for &(dst, cval) in const_edge_writes.iter() {
                        if let GprCopyLoc::Reg(r) = dst {
                            emit_const_u64(&mut words, r, cval);
                        }
                    }
                    for &(reg, copy_off) in byval_reg_writes.iter().take(byval_reg_n) {
                        emit_sp_addr(&mut words, reg, copy_off);
                    }
                    for &(reg, w) in sext_args.iter().take(sext_n) {
                        words.push(enc::sext(reg, reg, w));
                    }

                    if is_tail {
                        // Tail-Call Optimization: tear down frame (if any) and jump directly (`b` / `br`)!
                        emit_epilogue(&mut words);
                        if let Some(target) = direct_target {
                            let b_at = u32_of(words.len() * 4);
                            words.push(enc::b(0));
                            out.relocs.push(Reloc { at: b_at, in_relro: false, kind: RelocKind::Branch26, target, addend: 0 });
                        } else {
                            words.push(enc::br(X17));
                        }
                        // Skip the immediately following `Op::Return`!
                        inst += 2;
                        continue;
                    }

                    let call_at = if let Some(target) = direct_target {
                        match call_mode {
                            CallMode::DirectBl => {
                                // Release AOT export: single-instruction direct `bl` (zero dead NOPs)!
                                let bl_at = u32_of(words.len() * 4);
                                words.push(enc::bl(0));
                                out.relocs.push(Reloc { at: bl_at, in_relro: false, kind: RelocKind::Branch26, target, addend: 0 });
                                bl_at
                            }
                            CallMode::GotSlot => {
                                let adrp_at = u32_of(words.len() * 4);
                                words.push(enc::adrp(X16, 0));
                                words.push(enc::ldr(Size::X, X16, X16, 0));
                                out.relocs.push(Reloc { at: adrp_at, in_relro: false, kind: RelocKind::GotPage21, target, addend: 0 });
                                out.relocs.push(Reloc { at: adrp_at + 4, in_relro: false, kind: RelocKind::GotPageOff12, target, addend: 0 });
                                let blr_at = u32_of(words.len() * 4);
                                words.push(enc::blr(X16));
                                blr_at
                            }
                        }
                    } else {
                        let blr_at = u32_of(words.len() * 4);
                        words.push(enc::blr(X17));
                        blr_at
                    };
                    if let Some((_, unwind_blk)) = invoke_blocks {
                        pending_unwind.push((call_at, Some(unwind_blk)));
                    } else if !at_bool(&is_cleanup_block, b) {
                        // Calls inside cleanup code intentionally get NO unwind row (Rust panic-in-drop aborts).
                        pending_unwind.push((call_at, None));
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
                        } else if ty == Ty::I128 {
                            match loc {
                                AllocLoc::GprPair(lo, hi) => {
                                    gpr_moves.clear();
                                    gpr_moves.push((GprCopyLoc::Reg(u32::from(lo)), GprCopyLoc::Reg(X0)));
                                    gpr_moves.push((GprCopyLoc::Reg(u32::from(hi)), GprCopyLoc::Reg(X1)));
                                    emit_parallel_gpr_moves(&mut words, &mut gpr_moves, spill_base)?;
                                }
                                AllocLoc::GprPairSpill(s0, s1) => {
                                    emit_st_spill_gpr(&mut words, X0, spill_base, s0);
                                    emit_st_spill_gpr(&mut words, X1, spill_base, s1);
                                }
                                _ => {}
                            }
                        } else if is_gpr_pair_ty(&view, ty) {
                            let is_blaze_res_call = view
                                .sigs
                                .get(sig_id.index())
                                .is_some_and(|r| Conv::from_byte(r.conv) == Some(Conv::BlazeResult));
                            let fields = view.members(ty);
                            let w0 = Ty(fields[0]).bits(64);
                            let w1 = Ty(fields[1]).bits(64);
                            emit_narrow(&mut words, X0, w0);
                            if is_blaze_res_call && at_bool(&blaze_result_br_fused, inst) {
                                // Carry flag `C` goes straight to the fused `b.cs` — do NOT emit `cset` or touch NZCV!
                                match loc {
                                    AllocLoc::GprPair(lo, _hi) => {
                                        let rlo = u32::from(lo);
                                        if rlo != X0 {
                                            words.push(enc::mov(rlo, X0));
                                        }
                                    }
                                    AllocLoc::GprPairSpill(s0, _s1) => {
                                        emit_st_spill_gpr(&mut words, X0, spill_base, s0);
                                    }
                                    _ => {}
                                }
                            } else {
                                if is_blaze_res_call {
                                    // Materialize Carry flag (`C=1` = Err) into `X1` via `cset x1, hs`.
                                    words.push(enc::cset(X1, Cond::Hs));
                                } else {
                                    emit_narrow(&mut words, X1, w1);
                                }
                                match loc {
                                    AllocLoc::GprPair(lo, hi) => {
                                        gpr_moves.clear();
                                        gpr_moves.push((GprCopyLoc::Reg(u32::from(lo)), GprCopyLoc::Reg(X0)));
                                        gpr_moves.push((GprCopyLoc::Reg(u32::from(hi)), GprCopyLoc::Reg(X1)));
                                        emit_parallel_gpr_moves(&mut words, &mut gpr_moves, spill_base)?;
                                    }
                                    AllocLoc::GprPairSpill(s0, s1) => {
                                        emit_st_spill_gpr(&mut words, X0, spill_base, s0);
                                        emit_st_spill_gpr(&mut words, X1, spill_base, s1);
                                    }
                                    _ => {}
                                }
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
                    if let Some((normal_blk, _)) = invoke_blocks
                        && next_block != Some(to_idx(normal_blk))
                    {
                        let widx = words.len();
                        words.push(enc::b(0));
                        patches.push(BranchPatch { word_idx: widx, kind: PatchKind::B, target_block: normal_blk });
                    }
                }
                Op::Resume => {
                    let exc_v = at_u32(view.data, inst);
                    let rs = load_gpr(&mut words, exc_v, X0);
                    if rs != X0 {
                        words.push(enc::mov(X0, rs));
                    }
                    let target = resolve_c_routine("_Unwind_Resume", &mut out);
                    let call_at = match call_mode {
                        CallMode::DirectBl => {
                            let bl_at = u32_of(words.len() * 4);
                            words.push(enc::bl(0));
                            out.relocs.push(Reloc { at: bl_at, in_relro: false, kind: RelocKind::Branch26, target, addend: 0 });
                            bl_at
                        }
                        CallMode::GotSlot => {
                            let adrp_at = u32_of(words.len() * 4);
                            words.push(enc::adrp(X16, 0));
                            words.push(enc::ldr(Size::X, X16, X16, 0));
                            out.relocs.push(Reloc { at: adrp_at, in_relro: false, kind: RelocKind::GotPage21, target, addend: 0 });
                            out.relocs.push(Reloc { at: adrp_at + 4, in_relro: false, kind: RelocKind::GotPageOff12, target, addend: 0 });
                            let blr_at = u32_of(words.len() * 4);
                            words.push(enc::blr(X16));
                            blr_at
                        }
                    };
                    // `_Unwind_Resume` ALWAYS gets a `Continue` row, even inside cleanup code (libunwind phase 2!).
                    pending_unwind.push((call_at, None));
                }
                Op::Jump => {
                    let at = to_idx(at_u32(view.data, inst));
                    let target_block = at_u32(view.extra, at);
                    let argc = to_idx(at_u32(view.extra, at + 1));
                    emit_edge_moves(&mut words, &mut gpr_moves, &mut fp_moves, &mut const_edge_writes, target_block, at + 2, argc);
                    if next_block != Some(to_idx(target_block)) {
                        let widx = words.len();
                        words.push(enc::b(0));
                        patches.push(BranchPatch { word_idx: widx, kind: PatchKind::B, target_block });
                    }
                }
                Op::Br => {
                    let at = to_idx(at_u32(view.data, inst));
                    let cond_v = at_u32(view.extra, at);
                    let then_block = at_u32(view.extra, at + 1);
                    let then_argc = to_idx(at_u32(view.extra, at + 2));
                    let else_at = at + 3 + then_argc;
                    let else_block = at_u32(view.extra, else_at);
                    let else_argc = to_idx(at_u32(view.extra, else_at + 1));

                    if then_argc == 0 && else_argc == 0 {
                        match at_fusion(&fusion, inst) {
                            Fusion::TbzBr { val, bit, nonzero_is_then, .. } => {
                                let rv = load_gpr(&mut words, val, X16);
                                if next_block == Some(to_idx(else_block)) {
                                    let kind = if nonzero_is_then { PatchKind::Tbnz(rv, bit) } else { PatchKind::Tbz(rv, bit) };
                                    let widx = words.len();
                                    words.push(enc::tbz(rv, bit, 0));
                                    patches.push(BranchPatch { word_idx: widx, kind, target_block: then_block });
                                } else if next_block == Some(to_idx(then_block)) {
                                    let kind = if nonzero_is_then { PatchKind::Tbz(rv, bit) } else { PatchKind::Tbnz(rv, bit) };
                                    let widx = words.len();
                                    words.push(enc::tbz(rv, bit, 0));
                                    patches.push(BranchPatch { word_idx: widx, kind, target_block: else_block });
                                } else {
                                    let kind = if nonzero_is_then { PatchKind::Tbnz(rv, bit) } else { PatchKind::Tbz(rv, bit) };
                                    let w1 = words.len();
                                    words.push(enc::tbz(rv, bit, 0));
                                    patches.push(BranchPatch { word_idx: w1, kind, target_block: then_block });
                                    let w2 = words.len();
                                    words.push(enc::b(0));
                                    patches.push(BranchPatch { word_idx: w2, kind: PatchKind::B, target_block: else_block });
                                }
                            }
                            Fusion::TstBr { a, rhs, imm_nrs, nonzero_is_then } => {
                                let ra = load_gpr(&mut words, a, X16);
                                if let Some(nrs) = imm_nrs {
                                    words.push(enc::ands_imm(ZR, ra, nrs));
                                } else {
                                    let rb = load_gpr(&mut words, rhs, X17);
                                    words.push(enc::ands(ZR, ra, rb));
                                }
                                let cond = if nonzero_is_then { Cond::Ne } else { Cond::Eq };
                                emit_cond_branch_pair(&mut words, &mut patches, cond, then_block, else_block, next_block);
                            }
                            Fusion::CmpBr { cond, a, b: bv, op_ty } => {
                                let is_zero_cmp = at_opt(&const_u64, to_idx(bv)) == Some(0)
                                    && matches!(cond, IntCond::Eq | IntCond::Ne);
                                if is_zero_cmp {
                                    let ra = load_gpr(&mut words, a, X16);
                                    if next_block == Some(to_idx(else_block)) {
                                        let kind = if cond == IntCond::Eq { PatchKind::Cbz(ra) } else { PatchKind::Cbnz(ra) };
                                        let widx = words.len();
                                        words.push(enc::cbz(ra, 0));
                                        patches.push(BranchPatch { word_idx: widx, kind, target_block: then_block });
                                    } else if next_block == Some(to_idx(then_block)) {
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
                                    emit_cond_branch_pair(&mut words, &mut patches, cond_of(cond), then_block, else_block, next_block);
                                }
                            }
                            Fusion::FcmpBr { cond, a, b: bv, double } => {
                                let va = load_fp(&mut words, a, V30);
                                let vb = load_fp(&mut words, bv, V31);
                                words.push(enc::fcmp(double, va, vb));
                                emit_cond_branch_pair(&mut words, &mut patches, cond, then_block, else_block, next_block);
                            }
                            Fusion::BlazeResultCallBr => {
                                // Single-instruction `b.cs err_blk` directly on the callee's Carry flag (`C=1` = Err)!
                                emit_cond_branch_pair(&mut words, &mut patches, Cond::Hs, then_block, else_block, next_block);
                            }
                            _ => {
                                let rc = load_gpr(&mut words, cond_v, X16);
                                if next_block == Some(to_idx(else_block)) {
                                    let widx = words.len();
                                    words.push(enc::cbnz(rc, 0));
                                    patches.push(BranchPatch { word_idx: widx, kind: PatchKind::Cbnz(rc), target_block: then_block });
                                } else if next_block == Some(to_idx(then_block)) {
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
                        let skip_then = match at_fusion(&fusion, inst) {
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
                            Fusion::BlazeResultCallBr => {
                                // Skip `then` (Err) edge moves when Carry Clear (`Ok`, `Cond::Lo` = `b.cc`)!
                                let inv = Cond::Lo;
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
                        let after_skip = words.len();
                        emit_edge_moves(&mut words, &mut gpr_moves, &mut fp_moves, &mut const_edge_writes, then_block, at + 3, then_argc);
                        if words.len() == after_skip {
                            // Zero `then`-edge moves after register coalescing: replace `b.!cond skip; b then`
                            // with a single direct conditional branch `b.cond then` falling straight into `else`!
                            let direct_kind = match skip_then.1 {
                                PatchKind::BCond(inv) => PatchKind::BCond(invert_cond(inv)),
                                PatchKind::Cbz(rc) => PatchKind::Cbnz(rc),
                                _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unexpected skip_then patch kind" }),
                            };
                            patches.push(BranchPatch {
                                word_idx: skip_then.0,
                                kind: direct_kind,
                                target_block: then_block,
                            });
                        } else {
                            let w_then = words.len();
                            words.push(enc::b(0));
                            patches.push(BranchPatch { word_idx: w_then, kind: PatchKind::B, target_block: then_block });

                            let else_start = words.len();
                            let delta = i32_of(else_start - skip_then.0);
                            let skip_word = match skip_then.1 {
                                PatchKind::BCond(inv) => enc::b_cond(inv, delta),
                                PatchKind::Cbz(rc) => enc::cbz(rc, delta),
                                _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unexpected skip_then patch kind" }),
                            };
                            set_at(&mut words, skip_then.0, skip_word);
                        }
                        emit_edge_moves(&mut words, &mut gpr_moves, &mut fp_moves, &mut const_edge_writes, else_block, else_at + 2, else_argc);
                        if next_block != Some(to_idx(else_block)) {
                            let w_else = words.len();
                            words.push(enc::b(0));
                            patches.push(BranchPatch { word_idx: w_else, kind: PatchKind::B, target_block: else_block });
                        }
                    }
                }
                Op::Switch => {
                    let at = to_idx(at_u32(view.data, inst));
                    let scrutinee = at_u32(view.extra, at);
                    let default_block = at_u32(view.extra, at + 1);
                    let default_argc = to_idx(at_u32(view.extra, at + 2));
                    let cases_at = at + 3 + default_argc;
                    let num_cases = to_idx(at_u32(view.extra, cases_at));
                    let width_words = to_idx(ty_at(view.tys, to_idx(scrutinee)).const_words());

                    let sr = load_gpr(&mut words, scrutinee, X16);

                    // Dense O(1) PC-relative jump table (`adr` + `ldrsw [tab, idx, lsl #2]` + `add` + `br`),
                    // supporting both 0-arg direct targets and block-param-carrying edges via compact inline trampolines!
                    if let Some((min_v, span)) = dense_switch_plan(&view, inst) {
                        // `(target_block, args_start, argc)` per dense table entry; default fills holes.
                        let mut entries: Vec<(u32, usize, usize), Alloc<'a>> = Vec::with_capacity_in(span, alloc);
                        for _ in 0..span {
                            entries.push((default_block, at + 3, default_argc));
                        }
                        let mut pos = cases_at + 1;
                        for _ in 0..num_cases {
                            let mut case_val = 0u64;
                            for w in (0..width_words).rev() {
                                case_val = (case_val << 32) | u64::from(at_u32(view.extra, pos + w));
                            }
                            pos += width_words;
                            let case_block = at_u32(view.extra, pos);
                            let case_argc = to_idx(at_u32(view.extra, pos + 1));
                            let case_args_start = pos + 2;
                            pos += 2 + case_argc;
                            set_at(&mut entries, to_idx(case_val - min_v), (case_block, case_args_start, case_argc));
                        }
                        // Compute `idx = sr - min_v` in `X16`.
                        if min_v == 0 {
                            if sr != X16 {
                                words.push(enc::mov(X16, sr));
                            }
                        } else if min_v <= 4095 {
                            words.push(enc::sub_imm(X16, sr, trunc_u32(min_v), false));
                        } else {
                            emit_const_u64(&mut words, X17, min_v);
                            words.push(enc::sub(X16, sr, X17));
                        }
                        // Exhaustive enum niche switch: if `default_block` is `Op::Unreachable`-only,
                        // skip the `cmp + b.hi` bounds check completely (saves 2 ops + 1 BTB entry)!
                        let default_is_unreachable = default_argc == 0
                            && view
                                .block(Block(default_block))
                                .find(|&i| view.op(i) != Op::Param)
                                .is_some_and(|i| view.op(i) == Op::Unreachable);
                        let b_def = if default_is_unreachable {
                            None
                        } else {
                            words.push(enc::cmp_imm(X16, u32_of(span - 1)));
                            let idx = words.len();
                            words.push(enc::b_cond(Cond::Hi, 0));
                            if default_argc == 0 {
                                patches.push(BranchPatch {
                                    word_idx: idx,
                                    kind: PatchKind::BCond(Cond::Hi),
                                    target_block: default_block,
                                });
                            }
                            Some(idx)
                        };
                        // `adr x17, table` (4 instructions ahead = +16 bytes).
                        words.push(enc::adr(X17, 16));
                        words.push(enc::ldrsw_reg_scaled(X16, X17, X16));
                        words.push(enc::add(X17, X17, X16));
                        words.push(enc::br(X17));
                        let table_start_word = words.len();
                        for _ in 0..span {
                            words.push(0);
                        }
                        // Emit default trampoline once if `default_argc > 0`.
                        let mut def_tramp_word: Option<usize> = None;
                        if default_argc > 0 {
                            let tw = words.len();
                            def_tramp_word = Some(tw);
                            if let Some(b_def_idx) = b_def {
                                set_at(&mut words, b_def_idx, enc::b_cond(Cond::Hi, i32_of(tw - b_def_idx)));
                            }
                            emit_edge_moves(&mut words, &mut gpr_moves, &mut fp_moves, &mut const_edge_writes, default_block, at + 3, default_argc);
                            let wb = words.len();
                            words.push(enc::b(0));
                            patches.push(BranchPatch { word_idx: wb, kind: PatchKind::B, target_block: default_block });
                        }
                        for (k, &(tb, astart, argc)) in entries.iter().enumerate() {
                            let eidx = table_start_word + k;
                            if argc == 0 {
                                patches.push(BranchPatch {
                                    word_idx: eidx,
                                    kind: PatchKind::JumpTableEntry { table_start_word },
                                    target_block: tb,
                                });
                            } else if tb == default_block && astart == at + 3 && argc == default_argc {
                                let tw = def_tramp_word.ok_or(TierError::Refused {
                                    tier: TIER,
                                    inst: Some(u32_of(inst)),
                                    what: "missing default switch trampoline",
                                })?;
                                set_at(&mut words, eidx, word_delta(tw, table_start_word).wrapping_mul(4).cast_unsigned());
                            } else {
                                let tw = words.len();
                                emit_edge_moves(&mut words, &mut gpr_moves, &mut fp_moves, &mut const_edge_writes, tb, astart, argc);
                                if words.len() == tw {
                                    // Edge was fully coalesced (0 moves!): jump straight to `tb`!
                                    patches.push(BranchPatch {
                                        word_idx: eidx,
                                        kind: PatchKind::JumpTableEntry { table_start_word },
                                        target_block: tb,
                                    });
                                } else {
                                    let wb = words.len();
                                    words.push(enc::b(0));
                                    patches.push(BranchPatch { word_idx: wb, kind: PatchKind::B, target_block: tb });
                                    set_at(&mut words, eidx, word_delta(tw, table_start_word).wrapping_mul(4).cast_unsigned());
                                }
                            }
                        }
                        inst += 1;
                        continue;
                    }

                    let mut pos = cases_at + 1;
                    for _ in 0..num_cases {
                        let mut case_val = 0u64;
                        for w in (0..width_words).rev() {
                            case_val = (case_val << 32) | u64::from(at_u32(view.extra, pos + w));
                        }
                        pos += width_words;
                        let case_block = at_u32(view.extra, pos);
                        let case_argc = to_idx(at_u32(view.extra, pos + 1));
                        let case_args_start = pos + 2;
                        pos += 2 + case_argc;

                        if case_val <= 4095 {
                            words.push(enc::cmp_imm(sr, trunc_u32(case_val)));
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
                            set_at(&mut words, skip_idx, enc::b_cond(Cond::Ne, i32_of(after - skip_idx)));
                        }
                    }
                    emit_edge_moves(&mut words, &mut gpr_moves, &mut fp_moves, &mut const_edge_writes, default_block, at + 3, default_argc);
                    if next_block != Some(to_idx(default_block)) {
                        let widx = words.len();
                        words.push(enc::b(0));
                        patches.push(BranchPatch { word_idx: widx, kind: PatchKind::B, target_block: default_block });
                    }
                }
                Op::Return => {
                    let at = to_idx(at_u32(view.data, inst));
                    let retc = to_idx(at_u32(view.extra, at));
                    let is_blaze_result = view
                        .sigs
                        .get(SigId(view.head.sig).index())
                        .and_then(|r| Conv::from_byte(r.conv))
                        == Some(Conv::BlazeResult);
                    if retc == 2 && is_blaze_result {
                        // Zero-branch `Conv::BlazeResult`: `val` in `X0`, `is_err` in Carry flag `C`
                        // (`C = 0` on Ok, `C = 1` on Err); preserved across epilogue + `ret` (RAS-friendly!).
                        let val_v = at_u32(view.extra, at + 1);
                        let err_v = at_u32(view.extra, at + 2);
                        let const_err = at_opt(&const_u64, to_idx(err_v));
                        let rerr = if const_err.is_some() {
                            ZR
                        } else {
                            let r = load_gpr(&mut words, err_v, X16);
                            if r == X0 {
                                words.push(enc::mov(X16, X0));
                                X16
                            } else {
                                r
                            }
                        };
                        match at_loc(&reg_alloc.loc_of, to_idx(val_v)) {
                            AllocLoc::Gpr(s) => {
                                if u32::from(s) != X0 {
                                    words.push(enc::mov(X0, u32::from(s)));
                                }
                            }
                            AllocLoc::GprSpill(s) => emit_ld_spill_gpr(&mut words, X0, spill_base, s),
                            AllocLoc::RematConst(cval) => emit_const_u64(&mut words, X0, cval),
                            AllocLoc::None => {
                                let cval = at_opt(&const_u64, to_idx(val_v)).ok_or(TierError::Refused {
                                    tier: TIER,
                                    inst: Some(u32_of(inst)),
                                    what: "unallocated non-const blaze_result val",
                                })?;
                                emit_const_u64(&mut words, X0, cval);
                            }
                            _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unexpected blaze_result val return loc" }),
                        }
                        if Ext::from_byte(func_sig.rets[0].ext) == Some(Ext::Sext) {
                            let w = ty_at(view.tys, to_idx(val_v)).bits(64);
                            if w < 64 {
                                words.push(enc::sext(X0, X0, w));
                            }
                        }
                        match const_err {
                            Some(0) => words.push(enc::adds(ZR, ZR, ZR)), // 0 + 0 => C = 0 (Ok)
                            Some(_) => words.push(enc::cmp(ZR, ZR)),      // 0 - 0 => C = 1 (Err)
                            None => words.push(enc::cmp_imm(rerr, 1)),    // rerr >= 1 => C = 1, rerr == 0 => C = 0
                        }
                    } else if retc == 2 {
                        // 2-word integer/pointer return in `X0:X1`!
                        gpr_moves.clear();
                        const_edge_writes.clear();
                        for ridx in 0..2usize {
                            let rv = at_u32(view.extra, at + 1 + ridx);
                            let dst = GprCopyLoc::Reg(if ridx == 0 { X0 } else { X1 });
                            match at_loc(&reg_alloc.loc_of, to_idx(rv)) {
                                AllocLoc::Gpr(s) => gpr_moves.push((dst, GprCopyLoc::Reg(u32::from(s)))),
                                AllocLoc::GprSpill(s) => gpr_moves.push((dst, GprCopyLoc::Spill(s))),
                                AllocLoc::RematConst(cval) => const_edge_writes.push((dst, cval)),
                                AllocLoc::None => {
                                    let cval = at_opt(&const_u64, to_idx(rv)).ok_or(TierError::Refused {
                                        tier: TIER,
                                        inst: Some(u32_of(inst)),
                                        what: "unallocated non-const 2-word return",
                                    })?;
                                    const_edge_writes.push((dst, cval));
                                }
                                _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unexpected 2-word return loc" }),
                            }
                        }
                        emit_parallel_gpr_moves(&mut words, &mut gpr_moves, spill_base)?;
                        for &(dst, cval) in const_edge_writes.iter() {
                            if let GprCopyLoc::Reg(r) = dst {
                                emit_const_u64(&mut words, r, cval);
                            }
                        }
                        for (ridx, ret) in func_sig.rets.iter().take(2).enumerate() {
                            if Ext::from_byte(ret.ext) == Some(Ext::Sext) {
                                let rv = at_u32(view.extra, at + 1 + ridx);
                                let w = ty_at(view.tys, to_idx(rv)).bits(64);
                                if w < 64 {
                                    let reg = if ridx == 0 { X0 } else { X1 };
                                    words.push(enc::sext(reg, reg, w));
                                }
                            }
                        }
                    } else if retc == 1 {
                        let rv = at_u32(view.extra, at + 1);
                        let rv_ty = ty_at(view.tys, to_idx(rv));
                        if let Some(double) = is_fp_ty(rv_ty) {
                            match at_loc(&reg_alloc.loc_of, to_idx(rv)) {
                                AllocLoc::Fp(s) => {
                                    if u32::from(s) != 0 {
                                        words.push(enc::fmov_reg(double, 0, u32::from(s)));
                                    }
                                }
                                AllocLoc::FpSpill(s) => emit_ld_spill_fp(&mut words, 0, spill_base, s),
                                _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unexpected FP return loc" }),
                            }
                        } else if rv_ty == Ty::I128 {
                            gpr_moves.clear();
                            match at_loc(&reg_alloc.loc_of, to_idx(rv)) {
                                AllocLoc::GprPair(s0, s1) => {
                                    gpr_moves.push((GprCopyLoc::Reg(X0), GprCopyLoc::Reg(u32::from(s0))));
                                    gpr_moves.push((GprCopyLoc::Reg(X1), GprCopyLoc::Reg(u32::from(s1))));
                                }
                                AllocLoc::GprPairSpill(s0, s1) => {
                                    gpr_moves.push((GprCopyLoc::Reg(X0), GprCopyLoc::Spill(s0)));
                                    gpr_moves.push((GprCopyLoc::Reg(X1), GprCopyLoc::Spill(s1)));
                                }
                                _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unexpected i128 return loc" }),
                            }
                            emit_parallel_gpr_moves(&mut words, &mut gpr_moves, spill_base)?;
                        } else {
                            match at_loc(&reg_alloc.loc_of, to_idx(rv)) {
                                AllocLoc::Gpr(s) => {
                                    if u32::from(s) != X0 {
                                        words.push(enc::mov(X0, u32::from(s)));
                                    }
                                }
                                AllocLoc::GprSpill(s) => emit_ld_spill_gpr(&mut words, X0, spill_base, s),
                                AllocLoc::RematConst(cval) => emit_const_u64(&mut words, X0, cval),
                                AllocLoc::None => {
                                    let cval = at_opt(&const_u64, to_idx(rv)).ok_or(TierError::Refused {
                                        tier: TIER,
                                        inst: Some(u32_of(inst)),
                                        what: "unallocated non-const return",
                                    })?;
                                    emit_const_u64(&mut words, X0, cval);
                                }
                                _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unexpected GPR return loc" }),
                            }
                            if Ext::from_byte(func_sig.rets[0].ext) == Some(Ext::Sext) {
                                let w = rv_ty.bits(64);
                                if w < 64 {
                                    words.push(enc::sext(X0, X0, w));
                                }
                            }
                        }
                    }
                    emit_epilogue(&mut words);
                    words.push(enc::ret());
                }
                Op::Trap => {
                    words.push(enc::brk(at_u32(view.data, inst)));
                }
                Op::Unreachable => {
                    let ord = at_u32(&unreach_ord, inst);
                    if ord != u32::MAX {
                        out.traps.push(TrapRow {
                            at: u32_of(words.len() * 4),
                            kind: TrapKind::Intrinsic,
                            site: Some((0, ord)),
                            held: Held::Unknown,
                            subject: None,
                        });
                    }
                    words.push(enc::brk(BRK_UNREACHABLE));
                }
                _ => {
                    return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: op.name() });
                }
            }
            inst += 1;
        }
    }
    if let Some(err) = internal_err.get() {
        return Err(err);
    }

    // 5. Patch all intra-function branch displacements & jump table entries.
    for p in &patches {
        let target_w = at_usize(&block_word_off, to_idx(p.target_block));
        let delta = word_delta(target_w, p.word_idx);
        set_at(&mut words, p.word_idx, match p.kind {
            PatchKind::B => enc::b(delta),
            PatchKind::BCond(c) => enc::b_cond(c, delta),
            PatchKind::Cbz(r) => enc::cbz(r, delta),
            PatchKind::Cbnz(r) => enc::cbnz(r, delta),
            PatchKind::Tbz(r, bit) => enc::tbz(r, bit, delta),
            PatchKind::Tbnz(r, bit) => enc::tbnz(r, bit, delta),
            PatchKind::JumpTableEntry { table_start_word } => {
                word_delta(target_w, table_start_word).wrapping_mul(4).cast_unsigned()
            }
        });
    }

    // 5b. Finalize unwind rows now that every landing block's exact byte offset is known.
    for &(call_at, opt_landing) in pending_unwind.iter() {
        let row = match opt_landing {
            None => blaze_code::unwind::Row {
                start: call_at,
                len: 4,
                landing: 0,
                action: blaze_code::unwind::Action::Continue,
            },
            Some(lb) => {
                let action = match view.block_kind(Block(lb)) {
                    BlockKind::Cleanup => blaze_code::unwind::Action::Cleanup,
                    BlockKind::Catch => blaze_code::unwind::Action::Catch,
                    BlockKind::Terminate => blaze_code::unwind::Action::Terminate,
                    BlockKind::Normal | BlockKind::Cold => {
                        return Err(TierError::Refused {
                            tier: TIER,
                            inst: None,
                            what: "invoke unwind target not landing",
                        });
                    }
                };
                blaze_code::unwind::Row {
                    start: call_at,
                    len: 4,
                    landing: u32_of(at_usize(&block_word_off, to_idx(lb)) * 4),
                    action,
                }
            }
        };
        out.unwind.push(row);
        out.pads.push(row.pad());
    }

    // 6. Encode AArch64 words little-endian into `Emitted.code`.
    out.code.reserve(words.len() * 4);
    for &w in &words {
        out.code.extend_from_slice(&w.to_le_bytes());
    }
    Ok((out, reg_alloc.clobbers))
}

#[inline]
fn emit_cond_branch_pair<'a>(
    words: &mut Vec<u32, Alloc<'a>>,
    patches: &mut Vec<BranchPatch, Alloc<'a>>,
    cond: Cond,
    then_block: u32,
    else_block: u32,
    next_block: Option<usize>,
) {
    if next_block == Some(to_idx(else_block)) {
        let widx = words.len();
        words.push(enc::b_cond(cond, 0));
        patches.push(BranchPatch { word_idx: widx, kind: PatchKind::BCond(cond), target_block: then_block });
    } else if next_block == Some(to_idx(then_block)) {
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
