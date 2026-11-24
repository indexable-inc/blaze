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
use blaze_tier::{
    CallClobbers, Emitted, Named, Names, Outside, Reg as TierReg, RegSet, Reloc, RelocKind, TierError, TierName,
};

use super::CallMode;
use super::regalloc::{self, AllocLoc, ScratchOrder, is_fp_ty, is_gpr_pair_ty, is_vec128_ty, is_vec_mask_ty};
use super::release_opts::{
    MagicUdiv64, compute_block_order, compute_magic_udiv64, dense_switch_plan, detect_tail_calls,
};
use crate::enc::{
    self, Barrier, Cond, FP, Ftype, LR, Lse, Reg, SP, Size, X0, X1, X2, X8, X9, X10, X11, X12, X13, X14, X16, ZR,
};
use crate::{enc_probe, enc_tls};
use crate::pass::{
    BRK_DIV_OVERFLOW, BRK_DIV_ZERO, BRK_UNREACHABLE, Loc, Regs,
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
/// Maximum byte length for `Sota`'s inline overlap-safe NEON `Memmove` (load all into `V30`/`V31`, then store).
const SOTA_MEMMOVE_INLINE_MOST: u64 = 32;

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

/// ARM64 shifted-register shift kind for logical instructions (`AND`/`ORR`/`EOR`, C6.2.15/104/246).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LogShift {
    Lsl,
    Lsr,
    Asr,
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
    /// `and`/`or`/`xor(a, shl/lshr/ashr(b, k))` -> single `and`/`orr`/`eor rd, a, b, {lsl,lsr,asr} #k`.
    LogicalShifted { a: u32, b: u32, kind: LogShift, shift: u32, imm_inst: u32 },
    /// `xor(a, rotl/rotr(b, k))` -> `eor rd, a, b, ror #shift`.
    EorRor { a: u32, b: u32, shift: u32, imm_inst: u32 },
    /// `or`/`xor(shl(x, k), lshr(x, bits - k))` -> single `extr rd, x, x, #(bits - k)`.
    RorImm { x: u32, ror_amt: u32, imm1: u32, imm2: u32 },
    /// `and(lshr(src, lsb), (1 << width) - 1)` -> single `ubfx rd, src, #lsb, #width`.
    Ubfx { src: u32, lsb: u32, width: u32, shift_imm: u32, mask_imm: u32 },
    /// `or(and(dst, !((1 << width) - 1)), and(src [>> lsb], (1 << width) - 1))` -> `bfxil rd, src, #lsb, #width`.
    Bfxil {
        dst: u32,
        src: u32,
        lsb: u32,
        width: u32,
        dst_mask_imm: u32,
        src_mask_imm: u32,
        shift_imm: u32,
    },
    /// `br(icmp(cond, a, b), then, else)` -> fused `cmp`/`cbz` + `b.cond`.
    CmpBr { cond: IntCond, a: u32, b: u32, op_ty: Ty },
    /// `br(fcmp(cond, a, b), then, else)` -> fused `fcmp` + `b.cond`.
    FcmpBr { cond: Cond, a: u32, b: u32, double: bool },
    /// `select(icmp(cond, a, b), t, f)` -> fused `cmp` + `csel`/`fcsel`.
    CmpSelect { cond: IntCond, a: u32, b: u32, op_ty: Ty },
    /// `select(fcmp(cond, a, b), t, f)` -> fused `fcmp` + `csel`/`fcsel`.
    FcmpSelect { cond: Cond, a: u32, b: u32, double: bool },
    /// `select(shared_cmp, t, f)` reusing live NZCV flags from an earlier fused compare -> `csel`/`fcsel`.
    ReuseFlagsSelect { cond: Cond },
    /// `select(shared_cmp, rn, add(rm, 1))` reusing live NZCV flags -> `csinc rd, rn, rm, cond`.
    ReuseFlagsCsinc { cond: Cond, rn: u32, rm: u32, one_imm: u32 },
    /// `br(shared_cmp, then, else)` reusing live NZCV flags from an earlier fused compare -> `b.cond`.
    ReuseFlagsBr { cond: Cond, zero_imm: u32 },
    /// `sub(a, b)` emitting `subs` so a following `icmp(Eq/Ne, sub, 0) + br` reuses live NZCV flags!
    SubsFlags,
    /// `load`/`store` directly from `[sp, #off]` (folded `slot_addr`).
    MemSpOff { off: i64 },
    /// `load`/`store` directly from `[base, #off]` (folded `add`/`ptr_add` with constant).
    MemBaseOff { base: u32, off: i64, imm_inst: u32 },
    /// `load`/`store` directly from `[base, idx, lsl #size]` (scaled register index).
    MemScaledIdx { base: u32, idx: u32, shift_imm: u32 },
    /// `load`/`store` directly from `[base, widx, uxtw (#size)]` (32-bit unsigned-extended index).
    MemUxtwIdx { base: u32, idx: u32, scaled: bool, shift_imm: u32 },
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
    /// `select(icmp, rn, add(rm, 1))` or `select(icmp, add(rm, 1), rn)` -> `cmp` + `csinc`.
    CsincSelect { cond: IntCond, a: u32, b: u32, op_ty: Ty, rn: u32, rm: u32, inc_when_false: bool, one_imm: u32 },
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
    /// Widening `I128` multiply whose single-use `Zext(<=64 -> I128)` operand(s) read directly from
    /// their scalar source GPRs with zero `mov hi, xzr` and zero `madd`.
    I128WideMul { lhs_src: Option<u32>, rhs_src: Option<u32> },
    /// `Trunc(Mul(lhs, rhs): I128 -> <=64)` when the `I128` multiply is single-use: computes only
    /// the low 64-bit `mul` directly into the scalar destination with zero `umulh`, zero `madd`,
    /// zero `GprPair`, and zero `mov`.
    I128LoMul { lhs: u32, rhs: u32, lhs_src: Option<u32>, rhs_src: Option<u32> },
    /// `Trunc(Lshr/Ashr(x: I128, 64) -> <=64)` reading `x`'s upper 64-bit register directly with zero shift.
    I128HiTrunc { src: u32, shift_imm: u32 },
    /// `fmul(vec, splat(scalar))` -> 1-instruction NEON by-element `fmul.4s/2d vd, vn, vm.s/d[0]`!
    VecFmulElem { vec: u32, scalar: u32 },
    /// `fma(vec, splat(scalar), acc)` or `fadd(acc, fmul(vec, splat(scalar)))` -> `fmla.4s/2d vd, vn, vm.s/d[0]`!
    VecFmaddElem { vec: u32, scalar: u32, acc: u32 },
    /// `fsub(acc, fmul(vec, splat(scalar)))` -> `fmls.4s/2d vd, vn, vm.s/d[0]`!
    VecFmsubElem { vec: u32, scalar: u32, acc: u32 },
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

/// Deferred cold edge-move stub emitted at the end of the function so hot fallthrough edges
/// incur zero inline move instructions and zero taken branches.
#[derive(Clone, Copy, Debug)]
struct ColdEdgeStub {
    branch_word_idx: usize,
    branch_kind: PatchKind,
    target_block: u32,
    args_start: usize,
    args_len: usize,
}

/// Shared out-of-line PC-relative jump table placed at the end of the function for zero-move
/// dense `Op::Switch` sites sharing identical target blocks (e.g. `taildup` interpreter dispatch).
struct SharedJumpTable<'a> {
    targets: Vec<u32, Alloc<'a>>,
    adr_sites: Vec<usize, Alloc<'a>>,
}

/// Planned post-index `ldp`/`stp` group that folds a subsequent `PtrAdd(p, step)`.
#[derive(Clone, Copy, Debug)]
enum PostIndexPlan {
    /// Single post-index pair (`[rp], #step`) spanning `(first_inst, second_inst)`.
    Single {
        first_inst: usize,
        second_inst: usize,
        word: u32,
    },
    /// Two zero-dependency pairs emitted in strictly ascending address order at `i0`:
    /// `lo_word` (`[rp, #0]`) followed by `hi_word` (`[rp, #half]`), leaving `PtrAdd(p, step)`
    /// intact (`add rp, rp, #step`) so cache-line loads/stores stay strictly ascending with zero
    /// address-dependency chain.
    Quad {
        i0: usize,
        i1: usize,
        i2: usize,
        i3: usize,
        lo_word: u32,
        hi_word: u32,
    },
}

impl PostIndexPlan {
    #[inline]
    fn trigger_inst(self) -> usize {
        match self {
            Self::Single { first_inst, .. } => first_inst,
            Self::Quad { i0, .. } => i0,
        }
    }

    #[inline]
    fn contains(self, idx: usize) -> bool {
        match self {
            Self::Single { first_inst, second_inst, .. } => first_inst == idx || second_inst == idx,
            Self::Quad { i0, i1, i2, i3, .. } => idx == i0 || idx == i1 || idx == i2 || idx == i3,
        }
    }
}

/// Addressing mode for a pair emitted by post-index pair-advance fusion.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PairAddrMode {
    /// Post-index writeback by 1 pair width: `[rp], #32` (Vec128) or `[rp], #16` (Gpr64/Fp64).
    PostHalf,
    /// Signed offset 0 without writeback: `[rp, #0]`.
    OffZero,
    /// Signed offset without writeback at 1 pair width: `[rp, #32]` (Vec128) or `[rp, #16]` (Gpr64/Fp64).
    OffHalf,
}

/// Access width classification for post-index pair advance fusion.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PostAccessKind {
    Vec128,
    Gpr64,
    Fp64,
    Other,
}

/// True iff emitting `(op, ty, fus)` is strictly guaranteed to preserve ARM64 NZCV flags.
#[inline]
fn op_preserves_nzcv(op: Op, ty: Ty, fus: Fusion) -> bool {
    matches!(
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
            | Op::Zext
            | Op::Sext
            | Op::Trunc
            | Op::Bitcast
            | Op::Project
            | Op::Splat
            | Op::Extract
            | Op::Insert
            | Op::Shuffle
            | Op::Fpext
            | Op::Fptrunc
            | Op::SitoFp
            | Op::UitoFp
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
            | Op::Fma
            | Op::Fmuladd
            | Op::FminNum
            | Op::FmaxNum
            | Op::Fminimum
            | Op::Fmaximum
    ) || (ty != Ty::I128
        && matches!(
            op,
            Op::Add
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
                | Op::Clz
                | Op::Ctz
                | Op::Popcnt
                | Op::Bswap
                | Op::Bitrev
        )
        && !matches!(fus, Fusion::AdcsChain { .. } | Fusion::SubsFlags))
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

/// Deferred constant or global-address materialization emitted after parallel register moves.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RematWrite {
    Const(u64),
    Global(u32),
}

/// Pre-resolved relocation plan for a rematerialized `Op::DataAddr` / `Op::FuncAddr`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RematGlobalPlan {
    None,
    Got { target: u32 },
    OwnRelro { off: u32 },
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

/// Returns `Some(n)` when `inst` is an `Op::Memmove` with known constant byte length `n <= SOTA_MEMMOVE_INLINE_MOST`.
#[inline]
fn small_const_memmove_len(view: &View<'_>, inst: usize, const_u64: &[Option<u64>]) -> Option<u64> {
    if view.op(inst) != Op::Memmove {
        return None;
    }
    let at = to_idx(at_u32(view.data, inst));
    let len_v = to_idx(at_u32(view.extra, at + 2));
    at_opt(const_u64, len_v).filter(|&n| n <= SOTA_MEMMOVE_INLINE_MOST)
}

/// True iff an `I128` SSA value is provably zero in its upper 64 bits (`bits 64..128 == 0`).
#[inline]
fn i128_hi_is_zero(view: &View<'_>, v: u32) -> bool {
    let vi = to_idx(v);
    if ty_at(view.tys, vi) != Ty::I128 {
        return false;
    }
    match view.op(vi) {
        Op::Zext => {
            let src_ty = ty_at(view.tys, to_idx(at_u32(view.data, vi)));
            src_ty.is_scalar() && src_ty.is_int() && src_ty.bits(64) <= 64
        }
        Op::Iconst => at_u32(view.data, vi).cast_signed() >= 0,
        Op::IconstWide => {
            let at = to_idx(at_u32(view.data, vi));
            at_u32(view.extra, at + 2) == 0 && at_u32(view.extra, at + 3) == 0
        }
        _ => false,
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

/// `cnt vd.16b, vn.16b` (C7.2.47 CNT, Q=1): per-byte population count across 16 bytes.
#[inline]
const fn cnt_16b(rd: Reg, rn: Reg) -> u32 {
    0x4e20_5800 | (rn << 5) | rd
}

/// `shl vd.8h, vn.8h, #k` (C7.2.262 SHL immediate, Q=1, `1 <= k <= 15`).
#[inline]
const fn shl_8h_imm(rd: Reg, rn: Reg, k: u32) -> u32 {
    0x4f00_5400 | ((16 + (k & 0xf)) << 16) | (rn << 5) | rd
}

/// `ushr vd.8h, vn.8h, #k` (C7.2.382 USHR immediate, Q=1, `1 <= k <= 15`).
#[inline]
const fn ushr_8h_imm(rd: Reg, rn: Reg, k: u32) -> u32 {
    0x6f00_0400 | ((32 - (k & 0xf)) << 16) | (rn << 5) | rd
}

/// `uaddlp vd.<Ta>, vn.<Tb>` (C7.2.358 UADDLP, Q=1): unsigned pairwise add-long
/// (`size = 0`: `8h, 16b`; `size = 1`: `4s, 8h`; `size = 2`: `2d, 4s`).
#[inline]
const fn uaddlp_v(size: u32, rd: Reg, rn: Reg) -> u32 {
    0x6e20_2800 | (size << 22) | (rn << 5) | rd
}

/// `dup sd/dd, vn.s[lane]/vn.d[lane]` (C7.2.40 DUP element, scalar): `size = 2:S, 3:D`.
#[inline]
const fn dup_scalar_elem(size: u32, lane: u32, rd: Reg, rn: Reg) -> u32 {
    let imm5 = (lane << (size + 1)) | (1u32 << size);
    0x5e00_0400 | ((imm5 & 0x1f) << 16) | (rn << 5) | rd
}

/// `fmul vd.4s, vn.4s, vm.s[0]` / `fmul vd.2d, vn.2d, vm.d[0]` (C7.2.126 FMUL by element, Q=1, lane 0).
/// For `S` and `D` elements, `M:Rm` is a full 5-bit register index (`V0..V31`).
#[inline]
const fn fmul_elem0_v(double: bool, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    (if double { 0x4fc0_9000 } else { 0x4f80_9000 }) | (rm << 16) | (rn << 5) | rd
}

/// `fmla vd.4s, vn.4s, vm.s[0]` / `fmla vd.2d, vn.2d, vm.d[0]` (C7.2.104 FMLA by element, Q=1, lane 0).
#[inline]
const fn fmla_elem0_v(double: bool, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    (if double { 0x4fc0_1000 } else { 0x4f80_1000 }) | (rm << 16) | (rn << 5) | rd
}

/// `fmls vd.4s, vn.4s, vm.s[0]` / `fmls vd.2d, vn.2d, vm.d[0]` (C7.2.110 FMLS by element, Q=1, lane 0).
#[inline]
const fn fmls_elem0_v(double: bool, rd: Reg, rn: Reg, rm: Reg) -> u32 {
    (if double { 0x4fc0_5000 } else { 0x4f80_5000 }) | (rm << 16) | (rn << 5) | rd
}

/// `and`/`orr`/`eor rd, rn, rm, {lsl,lsr,asr} #shift` (C6.2.15/246/104, `N = 0`):
/// `opc = 0: AND, 1: ORR, 2: EOR`; `shift` in `0..=63` (`is64`) or `0..=31` (`!is64`).
#[inline]
const fn logical_shifted(is64: bool, opc: u32, kind: LogShift, rd: Reg, rn: Reg, rm: Reg, shift: u32) -> u32 {
    if is64 && opc == 2 {
        match kind {
            LogShift::Lsl => return enc::eor_lsl(rd, rn, rm, shift),
            LogShift::Lsr => return enc::eor_lsr(rd, rn, rm, shift),
            LogShift::Asr => {}
        }
    }
    let sh_bits = match kind {
        LogShift::Lsl => 0u32,
        LogShift::Lsr => 1u32,
        LogShift::Asr => 2u32,
    };
    let sf_bit = if is64 { 1u32 << 31 } else { 0u32 };
    let imm6 = if is64 { shift & 63 } else { shift & 31 };
    sf_bit | ((opc & 3) << 29) | 0x0a00_0000 | (sh_bits << 22) | (rm << 16) | (imm6 << 10) | (rn << 5) | rd
}

/// Emits a destructive 3-operand 128-bit NEON multiply-accumulate/sub (`vd = vc ± va * vb`),
/// assuming `va` was loaded with scratch `V29`, `vb` with `V30`, and `vc` with `V31`.
#[inline]
fn emit_vec_acc3(
    words: &mut Vec<u32, Alloc<'_>>,
    vd: Reg,
    va: Reg,
    vb: Reg,
    vc: Reg,
    acc_word: impl Fn(Reg, Reg, Reg) -> u32,
) {
    if vd == vc {
        words.push(acc_word(vd, va, vb));
    } else if vd != va && vd != vb {
        words.push(enc::orr_v(vd, vc, vc));
        words.push(acc_word(vd, va, vb));
    } else {
        if vc != V31 {
            words.push(enc::orr_v(V31, vc, vc));
        }
        words.push(acc_word(V31, va, vb));
        words.push(enc::orr_v(vd, V31, V31));
    }
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

/// Computes the exact register or stacked placement for parameter `p`, updating `(regs, nsaa)`
/// via the canonical AArch64 ABI register walk (`crate::pass::Regs`, matching `Reference`).
fn place_param(
    p: &blaze_lir::func::ParamRec,
    regs: &mut Regs,
    nsaa: &mut Nsaa,
) -> Option<ParamPlace> {
    if ParamKind::from_byte(p.kind) == Some(ParamKind::Sret) {
        return Some(ParamPlace::Sret);
    }
    let ty = Ty(p.ty);
    match regs.next(ty) {
        Some(Loc::V(r, _)) => Some(ParamPlace::Fp(r)),
        Some(Loc::Pair(lo)) => Some(ParamPlace::GprPair(lo, lo + 1)),
        Some(Loc::X(r)) => Some(ParamPlace::Gpr(r)),
        None => nsaa.place(ty).map(ParamPlace::Stack),
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
    let mut regs = Regs::new(os);
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
        if place_param(p, &mut regs, &mut nsaa).is_none() {
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
fn check_subset(view: &View<'_>, has_lse: bool, has_crc32: bool, os: crate::Os) -> Result<(), TierError> {
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
                | Op::Extract
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
                | Op::Popcnt
                | Op::UaddSat
                | Op::UsubSat
                | Op::SaddSat
                | Op::SsubSat => !vfp,
                Op::Umin | Op::Umax | Op::Smin | Op::Smax => !vfp,
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
                Op::Shl | Op::Lshr if !vfp && (vsize == 1 || vsize == 2 || vsize == 3) => {
                    let at = to_idx(at_u32(view.data, inst));
                    let rhs = to_idx(at_u32(view.extra, at + 1));
                    let max_sh = if vsize == 1 {
                        16
                    } else if vsize == 2 {
                        32
                    } else {
                        64
                    };
                    view.op(rhs) == Op::Splat
                        && extract_const_u64(view, to_idx(at_u32(view.data, rhs))).is_some_and(|k| k < max_sh)
                }
                Op::Insert if !vfp && (vsize == 2 || vsize == 3) => {
                    let at = to_idx(at_u32(view.data, inst));
                    let max_lane = if vsize == 2 { 4 } else { 2 };
                    at_u32(view.extra, at + 2) < max_lane
                }
                Op::Fadd | Op::Fsub | Op::Fmul | Op::Fma | Op::Fmuladd => vfp,
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
                Op::TargetIntrinsic => {
                    let at = to_idx(at_u32(view.data, inst));
                    let name = view.intrinsic_name(IntrinsicId(at_u32(view.extra, at)));
                    let argc = to_idx(at_u32(view.extra, at + 1));
                    (name == b"blaze.neon.tbl16" && argc == 2 && !vfp && vsize == 0)
                        || (name == b"blaze.neon.tbl64" && argc == 5 && !vfp && vsize == 0)
                        || ((name == b"blaze.neon.sdot" || name == b"blaze.neon.udot")
                            && argc == 3
                            && !vfp
                            && vsize == 2)
                }
                _ => false,
            };
            if !vec_ok {
                return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unsupported 128-bit NEON vector op" });
            }
        }
        if op == Op::Extract {
            let at = to_idx(at_u32(view.data, inst));
            let vec_v = to_idx(at_u32(view.extra, at));
            let lane = at_u32(view.extra, at + 1);
            let Some((vsize, _)) = is_vec128_ty(ty_at(view.tys, vec_v)) else {
                return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "non-128-bit vector extract" });
            };
            let max_lane = 16u32 >> vsize;
            if lane >= max_lane {
                return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "vector extract lane out of range" });
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
                let is_crc32_b = name == b"blaze.a64.crc32b" || name == b"blaze.a64.crc32cb";
                let is_crc32_x = name == b"blaze.a64.crc32x" || name == b"blaze.a64.crc32cx";
                let crc32_ok = has_crc32
                    && (is_crc32_b || is_crc32_x)
                    && argc == 2
                    && ty == Ty::I32
                    && ty_at(view.tys, to_idx(at_u32(view.extra, at + 2))) == Ty::I32
                    && {
                        let data_ty = ty_at(view.tys, to_idx(at_u32(view.extra, at + 3)));
                        if is_crc32_b {
                            data_ty == Ty::I32 || data_ty == Ty::I8
                        } else {
                            data_ty == Ty::I64
                        }
                    };
                let ok = (name == b"blaze.prefetch.r1" && argc == 1 && ty == Ty::NONE)
                    || crc32_ok
                    || ((name == b"blaze.sme2.gemm_f32"
                        || name == b"blaze.sme2.gemm_f64"
                        || name == b"blaze.sme2.gemm_i8_i32"
                        || name == b"blaze.sme2.gemm_u8_u32")
                        && argc == 7
                        && ty == Ty::NONE)
                    || (name == b"blaze.neon.tbl16" && argc == 2 && is_vec128_ty(ty).is_some())
                    || (name == b"blaze.neon.tbl64" && argc == 5 && is_vec128_ty(ty).is_some())
                    || ((name == b"blaze.neon.sdot" || name == b"blaze.neon.udot")
                        && argc == 3
                        && is_vec128_ty(ty).is_some());
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
    emit_sota_a64_with_abi(
        view,
        names,
        call_mode,
        features,
        is_macos,
        call_clobbers,
        None,
        false,
        alloc,
    )
}

/// Zero-ABI & IPRA-enabled SOTA AArch64 emitter: accepts optional interprocedural register-avoidance
/// [`ScratchOrder`] and `preserve_all_cold` (`CallConvKind::ColdPreserveAll`) so cold callees save
/// and restore every modified caller-saved GPR/FP outside their return slots and expose an empty
/// scratch clobber mask to hot callers.
pub(crate) fn emit_sota_a64_with_abi<'a>(
    view: View<'_>,
    names: &dyn Names,
    call_mode: CallMode,
    features: u64,
    is_macos: bool,
    call_clobbers: impl Fn(usize) -> CallClobbers,
    scratch_order: Option<&ScratchOrder>,
    preserve_all_cold: bool,
    alloc: Alloc<'a>,
) -> Result<(Emitted<'a>, CallClobbers), TierError> {
    let has_lse = features & blaze_tier::Machine::LSE != 0;
    let has_cssc = features & blaze_tier::Machine::CSSC != 0;
    let has_sha3 = features & blaze_tier::Machine::SHA3 != 0;
    let has_crc32 = features & blaze_tier::Machine::CRC32 != 0;
    let os = if is_macos { crate::Os::MacOs } else { crate::Os::Linux };
    check_subset(&view, has_lse, has_crc32, os)?;

    // Canonicalize into dominator-respecting RPO order before fusion, loop-depth analysis, and
    // linear-interval register allocation (`regalloc` requires `def_block <= use_block` for all
    // reachable blocks and `target <= src` for all natural loop back-edges).
    let rpo_owned = blaze_lir::coldsplit::reorder_blocks_rpo(view, alloc).ok().flatten();
    let view = rpo_owned.as_ref().map_or(view, |r| r.func.view());
    let call_clobbers = |new_inst: usize| -> CallClobbers {
        let orig = rpo_owned
            .as_ref()
            .and_then(|r| r.old_inst_of_new.get(new_inst).copied())
            .unwrap_or(new_inst);
        call_clobbers(orig)
    };

    let len = view.len();
    let blocks = view.blocks();
    let tail_calls = if preserve_all_cold {
        let mut tc: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
        for _ in 0..len {
            tc.push(false);
        }
        tc
    } else {
        detect_tail_calls(&view, call_mode == CallMode::DirectBl, os, alloc)
    };
    let block_order = compute_block_order(&view, alloc);
    let sole_switch_default = crate::pass::frame::sole_switch_defaults(view, alloc);
    // Functions past 64K LIR instructions can exceed ARM64's ±1 MiB `Imm19`/`adr` reach; emit
    // 2-instruction inverted conditional branches (`b.<!cond> +8; b target`, ±128 MiB reach) and
    // avoid `adr` jump tables so gigantic functions stay natively on SOTA with zero fallback.
    let far_branches = len >= 65_536;

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
            let mut regs = Regs::new(os);
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
                let _ = place_param(p, &mut regs, &mut nsaa);
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
        let mut regs = Regs::new(os);
        let mut nsaa = Nsaa::new(os);
        for p in view.sig(SigId(view.head.sig)).params {
            if matches!(place_param(p, &mut regs, &mut nsaa), Some(ParamPlace::Stack(_))) {
                has_stacked_in = true;
            }
        }
    }

    let byval_end = byval_base + byval_size;
    // Pack LIR stack slots by lifetime (`SlotLive`/`SlotDead`/provenance) using the shared frame
    // packer (`pass/frame.rs`). When the packed slots fit comfortably under half the 32 KiB
    // scaled-immediate window, place slots first (so Pattern A folds `load`/`store` directly into
    // `[sp, #imm]`); otherwise place spill slots first so large-frame functions still get up to
    // 4095 single-instruction spill slots instead of falling back with `max_spill_slots = 0`!
    let packed_zero = crate::pass::frame::pack_slots(view, 0, alloc)
        .map_err(|_| TierError::Refused { tier: TIER, inst: None, what: "stack frame >= 16 MiB" })?;
    let most_align = packed_zero.most_align;
    let slots_after_spills = byval_end.saturating_add(packed_zero.end) >= 16384;
    let (mut slot_at, slots_end_rel, spill_base, max_spill_slots) = if !slots_after_spills {
        let packed = crate::pass::frame::pack_slots(view, byval_end, alloc)
            .map_err(|_| TierError::Refused { tier: TIER, inst: None, what: "stack frame >= 16 MiB" })?;
        let spill_base_u64 = packed.end.next_multiple_of(8);
        if spill_base_u64 >= FRAME_LIMIT {
            return Err(TierError::Refused { tier: TIER, inst: None, what: "stack frame >= 16 MiB" });
        }
        let sb = u32_of(spill_base_u64);
        let max_spills = if sb < 32760 { (32760 - sb) / 8 } else { 0 };
        (packed.slot_at, 0u64, sb, max_spills)
    } else {
        let spill_base_u64 = byval_end.next_multiple_of(8);
        if spill_base_u64 >= FRAME_LIMIT {
            return Err(TierError::Refused { tier: TIER, inst: None, what: "stack frame >= 16 MiB" });
        }
        let sb = u32_of(spill_base_u64);
        let max_spills = if sb < 32760 { (32760 - sb) / 8 } else { 0 };
        (packed_zero.slot_at, packed_zero.end, sb, max_spills)
    };

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
    let mut ptr_off_foldable_uses: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    let mut ptr_shl_foldable_uses: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    let mut ptr_byte_foldable_uses: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    let mut shl_foldable_ptr_uses: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    let mut blaze_result_br_fused: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    for _ in 0..len {
        fusion.push(Fusion::None);
        skip_emit.push(false);
        slot_addr_folded_uses.push(0);
        ptr_off_foldable_uses.push(0);
        ptr_shl_foldable_uses.push(0);
        ptr_byte_foldable_uses.push(0);
        shl_foldable_ptr_uses.push(0);
        blaze_result_br_fused.push(false);
    }

    // Pre-pass: count foldable Load/Store pointer uses of:
    // - same-block `PtrAdd(base, const)` and 64-bit `Add(base, const)` (`MemBaseOff`)
    // - cross-block multi-use (1..=4 uses) non-volatile offset-0 `PtrAdd(base, Shl(idx, scale))`
    //   (including shared `Shl(idx, scale)` across 1..=4 foldable `PtrAdd`s like `keys[slot]` + `vals[slot]`)
    //   and byte `PtrAdd(base, idx)` (`MemScaledIdx` / `MemUxtwIdx` / `MemRegIdx`) whenever ALL uses fold!
    for inst in 0..len {
        let op = view.op(inst);
        if !matches!(op, Op::Load | Op::Store) {
            continue;
        }
        let at = to_idx(at_u32(view.data, inst));
        let (ptr, off, access_ty, store_val) = if op == Op::Load {
            (at_u32(view.extra, at), i64::from(at_u32(view.extra, at + 1).cast_signed()), ty_at(view.tys, inst), u32::MAX)
        } else {
            let val = at_u32(view.extra, at);
            (at_u32(view.extra, at + 1), i64::from(at_u32(view.extra, at + 2).cast_signed()), ty_at(view.tys, to_idx(val)), val)
        };
        if access_ty == Ty::I128 || store_val == ptr {
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
        let fits_off = |o: i64| -> bool {
            match mem_sz {
                None => (o >= 0 && o % 16 == 0 && o / 16 < 4096) || (-256..=255).contains(&o),
                Some(sz) => fits_mem_off(sz, o),
            }
        };
        let pi = to_idx(ptr);
        let pop = view.op(pi);
        let is_volatile = Flags(at_u8(view.flags, inst)).contains(Flags::VOLATILE);
        if !is_volatile && off == 0 && pop == Op::PtrAdd {
            let pi_uses = at_u32(&use_count, pi);
            if (1..=4).contains(&pi_uses)
                && let Some(sz) = mem_sz
            {
                let size_shift = mem_shift(sz);
                let pat = to_idx(at_u32(view.data, pi));
                let by = at_u32(view.extra, pat + 1);
                if ty_at(view.tys, to_idx(by)).bits(64) == 64 {
                    let bi = to_idx(by);
                    if size_shift == 0 {
                        if at_opt(&const_u64, bi).is_none() {
                            inc_u32(&mut ptr_byte_foldable_uses, pi);
                        }
                    } else if (1..=4).contains(&at_u32(&use_count, bi)) && view.op(bi) == Op::Shl {
                        let sat = to_idx(at_u32(view.data, bi));
                        let sh_v = at_u32(view.extra, sat + 1);
                        if at_opt(&const_u64, to_idx(sh_v)) == Some(u64::from(size_shift)) {
                            inc_u32(&mut ptr_shl_foldable_uses, pi);
                        }
                    }
                }
            }
        }
        if at_u32(&inst_block, pi) != at_u32(&inst_block, inst) {
            continue;
        }
        if pop == Op::PtrAdd {
            let pat = to_idx(at_u32(view.data, pi));
            let by = at_u32(view.extra, pat + 1);
            if let Some(cv) = at_opt(&const_u64, to_idx(by)) {
                let by_bits = ty_at(view.tys, to_idx(by)).bits(64);
                let delta = sign_extend_u64(cv, by_bits);
                if delta.checked_add(off).is_some_and(fits_off) {
                    inc_u32(&mut ptr_off_foldable_uses, pi);
                }
            }
        } else if pop == Op::Add && ty_at(view.tys, pi).bits(64) == 64 {
            let pat = to_idx(at_u32(view.data, pi));
            let a_lhs = at_u32(view.extra, pat);
            let a_rhs = at_u32(view.extra, pat + 1);
            let delta_opt = if let Some(cv) = at_opt(&const_u64, to_idx(a_rhs)) {
                Some(cv.cast_signed())
            } else if let Some(cv) = at_opt(&const_u64, to_idx(a_lhs)) {
                Some(cv.cast_signed())
            } else {
                None
            };
            if let Some(delta) = delta_opt {
                if delta.checked_add(off).is_some_and(fits_off) {
                    inc_u32(&mut ptr_off_foldable_uses, pi);
                }
            }
        }
    }

    // Count how many foldable scaled `PtrAdd`s (all of whose uses fold) consume each shared `Op::Shl`.
    for pi in 0..len {
        if view.op(pi) != Op::PtrAdd {
            continue;
        }
        let pi_uses = at_u32(&use_count, pi);
        if (1..=4).contains(&pi_uses) && at_u32(&ptr_shl_foldable_uses, pi) == pi_uses {
            let pat = to_idx(at_u32(view.data, pi));
            let base = at_u32(view.extra, pat);
            let by = at_u32(view.extra, pat + 1);
            let bi = to_idx(by);
            if base != by && view.op(bi) == Op::Shl && (1..=4).contains(&at_u32(&use_count, bi)) {
                inc_u32(&mut shl_foldable_ptr_uses, bi);
            }
        }
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
                                | Op::Invoke
                                | Op::InvokeIndirect
                                | Op::Udiv
                                | Op::Sdiv
                                | Op::Urem
                                | Op::Srem
                                | Op::Memcpy
                                | Op::Memmove
                                | Op::Memset
                                | Op::Memcmp
                                | Op::Frem
                                | Op::TlsAddr
                                | Op::AtomicRmw
                                | Op::Cmpxchg
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
                                | Op::Invoke
                                | Op::InvokeIndirect
                                | Op::Udiv
                                | Op::Sdiv
                                | Op::Urem
                                | Op::Srem
                                | Op::Memcpy
                                | Op::Memmove
                                | Op::Memset
                                | Op::Memcmp
                                | Op::Frem
                                | Op::TlsAddr
                                | Op::AtomicRmw
                                | Op::Cmpxchg
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
            Op::Add if is_vec128_ty(ty).is_some_and(|(vsize, vfp)| !vfp && vsize < 3) => {
                // Fuse 128-bit NEON integer `add(acc, mul(a, b))` / `add(mul(a, b), acc)` -> `mla.16b/8h/4s`!
                let at = to_idx(at_u32(view.data, inst));
                let lhs = at_u32(view.extra, at);
                let rhs = at_u32(view.extra, at + 1);
                let try_vmul = |prod: u32, acc: u32| -> Option<(u32, u32, u32)> {
                    let pi = to_idx(prod);
                    if at_u32(&use_count, pi) == 1
                        && at_u32(&inst_block, pi) == b
                        && view.op(pi) == Op::Mul
                        && !at_bool(&skip_emit, pi)
                    {
                        let pat = to_idx(at_u32(view.data, pi));
                        Some((at_u32(view.extra, pat), at_u32(view.extra, pat + 1), acc))
                    } else {
                        None
                    }
                };
                if let Some((a, mb, acc)) = try_vmul(lhs, rhs) {
                    set_at(&mut fusion, inst, Fusion::Madd { a, b: mb, acc });
                    set_at(&mut skip_emit, to_idx(lhs), true);
                    continue;
                }
                if let Some((a, mb, acc)) = try_vmul(rhs, lhs) {
                    set_at(&mut fusion, inst, Fusion::Madd { a, b: mb, acc });
                    set_at(&mut skip_emit, to_idx(rhs), true);
                    continue;
                }
            }
            Op::Sub if is_vec128_ty(ty).is_some_and(|(vsize, vfp)| !vfp && vsize < 3) => {
                // Fuse 128-bit NEON integer `sub(acc, mul(a, b))` -> `mls.16b/8h/4s`!
                let at = to_idx(at_u32(view.data, inst));
                let lhs = at_u32(view.extra, at);
                let rhs = at_u32(view.extra, at + 1);
                let ri = to_idx(rhs);
                if at_u32(&use_count, ri) == 1
                    && at_u32(&inst_block, ri) == b
                    && view.op(ri) == Op::Mul
                    && !at_bool(&skip_emit, ri)
                {
                    let pat = to_idx(at_u32(view.data, ri));
                    set_at(&mut fusion, inst, Fusion::Msub { a: at_u32(view.extra, pat), b: at_u32(view.extra, pat + 1), acc: lhs });
                    set_at(&mut skip_emit, ri, true);
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
            Op::Fsub
                if (is_fp_ty(ty).is_some() && Flags(at_u8(view.flags, inst)).contains(Flags::CONTRACT))
                    || (is_vec128_ty(ty).is_some_and(|(_, fp)| fp)
                        && (Flags(at_u8(view.flags, inst)).contains(Flags::CONTRACT)
                            || Flags(at_u8(view.flags, inst)).contains(Flags::REASSOC))) =>
            {
                let at = to_idx(at_u32(view.data, inst));
                let lhs = at_u32(view.extra, at);
                let rhs = at_u32(view.extra, at + 1);
                let ri = to_idx(rhs);
                let is_vec = is_vec128_ty(ty).is_some();
                let rflags = Flags(at_u8(view.flags, ri));
                let ok_flag = rflags.contains(Flags::CONTRACT) || (is_vec && rflags.contains(Flags::REASSOC));
                if at_u32(&use_count, ri) == 1
                    && at_u32(&inst_block, ri) == b
                    && view.op(ri) == Op::Fmul
                    && ok_flag
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
                if op == Op::And {
                    // Fuse `and(lshr(x, lsb), (1 << width) - 1)` -> 1-instruction `ubfx rd, x, #lsb, #width`!
                    let try_ubfx = |sh_cand: u32, mask_cand: u32| -> Option<(usize, Fusion)> {
                        let mask_v = at_opt(&const_u64, to_idx(mask_cand))?;
                        let width = is_low_bitmask(mask_v)?;
                        let si = to_idx(sh_cand);
                        if at_u32(&use_count, si) != 1
                            || at_u32(&inst_block, si) != b
                            || view.op(si) != Op::Lshr
                            || at_bool(&skip_emit, si)
                            || at_fusion(&fusion, si) != Fusion::None
                        {
                            return None;
                        }
                        let sat = to_idx(at_u32(view.data, si));
                        let src = at_u32(view.extra, sat);
                        let shift_imm = at_u32(view.extra, sat + 1);
                        let lsb_u64 = at_opt(&const_u64, to_idx(shift_imm))?;
                        if lsb_u64 >= u64::from(bits) {
                            return None;
                        }
                        let lsb = trunc_u32(lsb_u64);
                        if width >= 1 && lsb + width <= bits {
                            Some((
                                si,
                                Fusion::Ubfx {
                                    src,
                                    lsb,
                                    width,
                                    shift_imm,
                                    mask_imm: mask_cand,
                                },
                            ))
                        } else {
                            None
                        }
                    };
                    if let Some((si, fus)) = try_ubfx(lhs, rhs).or_else(|| try_ubfx(rhs, lhs)) {
                        set_at(&mut fusion, inst, fus);
                        set_at(&mut skip_emit, si, true);
                        continue;
                    }
                }
                if op == Op::Or {
                    // Fuse `or(and(dst, !mask), and(src [>> lsb], mask))` where `mask == (1 << w) - 1`
                    // -> `bfxil rd, src, #lsb, #w`!
                    let li = to_idx(lhs);
                    let ri = to_idx(rhs);
                    if at_u32(&use_count, li) == 1
                        && at_u32(&use_count, ri) == 1
                        && at_u32(&inst_block, li) == b
                        && at_u32(&inst_block, ri) == b
                        && !at_bool(&skip_emit, li)
                        && !at_bool(&skip_emit, ri)
                        && view.op(li) == Op::And
                        && view.op(ri) == Op::And
                    {
                        let type_mask = if bits == 64 { u64::MAX } else { 0xffff_ffffu64 };
                        let try_bfxil = |dst_and: usize, src_and: usize| -> Option<Fusion> {
                            if at_fusion(&fusion, dst_and) != Fusion::None {
                                return None;
                            }
                            let (src, lsb, width, src_mask_imm, shift_imm) = match at_fusion(&fusion, src_and) {
                                Fusion::Ubfx { src, lsb, width, shift_imm, mask_imm } => {
                                    (src, lsb, width, mask_imm, shift_imm)
                                }
                                Fusion::None => {
                                    let sat = to_idx(at_u32(view.data, src_and));
                                    let sa = at_u32(view.extra, sat);
                                    let sb = at_u32(view.extra, sat + 1);
                                    if let Some(w) = at_opt(&const_u64, to_idx(sb)).and_then(is_low_bitmask) {
                                        (sa, 0, w, sb, u32::MAX)
                                    } else if let Some(w) = at_opt(&const_u64, to_idx(sa)).and_then(is_low_bitmask) {
                                        (sb, 0, w, sa, u32::MAX)
                                    } else {
                                        return None;
                                    }
                                }
                                _ => return None,
                            };
                            if width == 0 || width >= bits || lsb + width > bits {
                                return None;
                            }
                            let low_mask = (1u64 << width) - 1;
                            let want_dst_mask = (!low_mask) & type_mask;
                            let dat = to_idx(at_u32(view.data, dst_and));
                            let da = at_u32(view.extra, dat);
                            let db = at_u32(view.extra, dat + 1);
                            let (dst, dst_mask_imm) = if at_opt(&const_u64, to_idx(db)) == Some(want_dst_mask) {
                                (da, db)
                            } else if at_opt(&const_u64, to_idx(da)) == Some(want_dst_mask) {
                                (db, da)
                            } else {
                                return None;
                            };
                            Some(Fusion::Bfxil {
                                dst,
                                src,
                                lsb,
                                width,
                                dst_mask_imm,
                                src_mask_imm,
                                shift_imm,
                            })
                        };
                        if let Some(fus) = try_bfxil(li, ri).or_else(|| try_bfxil(ri, li)) {
                            set_at(&mut fusion, inst, fus);
                            set_at(&mut skip_emit, li, true);
                            set_at(&mut skip_emit, ri, true);
                            continue;
                        }
                    }
                }
                if matches!(op, Op::Or | Op::Xor) {
                    // Fuse open-coded rotate `(x << k) | (x >> (bits - k))` -> 1-instruction `extr`/`extr_w`!
                    let li = to_idx(lhs);
                    let ri = to_idx(rhs);
                    if at_u32(&use_count, li) == 1
                        && at_u32(&use_count, ri) == 1
                        && at_u32(&inst_block, li) == b
                        && at_u32(&inst_block, ri) == b
                        && !at_bool(&skip_emit, li)
                        && !at_bool(&skip_emit, ri)
                        && at_fusion(&fusion, li) == Fusion::None
                        && at_fusion(&fusion, ri) == Fusion::None
                    {
                        let lop = view.op(li);
                        let rop = view.op(ri);
                        let sh_pair = if lop == Op::Shl && rop == Op::Lshr {
                            Some((li, ri))
                        } else if lop == Op::Lshr && rop == Op::Shl {
                            Some((ri, li))
                        } else {
                            None
                        };
                        if let Some((shl_i, lshr_i)) = sh_pair {
                            let lat = to_idx(at_u32(view.data, shl_i));
                            let rat = to_idx(at_u32(view.data, lshr_i));
                            let (lx, lk) = (at_u32(view.extra, lat), at_u32(view.extra, lat + 1));
                            let (rx, rk) = (at_u32(view.extra, rat), at_u32(view.extra, rat + 1));
                            if lx == rx {
                                if let (Some(k1), Some(k2)) = (at_opt(&const_u64, to_idx(lk)), at_opt(&const_u64, to_idx(rk))) {
                                    let b64 = u64::from(bits);
                                    if k1 > 0 && k1 < b64 && k1 + k2 == b64 {
                                        set_at(&mut fusion, inst, Fusion::RorImm {
                                            x: lx,
                                            ror_amt: trunc_u32(k2),
                                            imm1: lk,
                                            imm2: rk,
                                        });
                                        set_at(&mut skip_emit, li, true);
                                        set_at(&mut skip_emit, ri, true);
                                        continue;
                                    }
                                }
                            }
                        }
                    }
                }
                if op == Op::Xor {
                    // Fuse `xor(a, rotl/rotr(b, k))` -> 1-instruction `eor rd, ra, rb, ror #ror_amt`!
                    let try_xor_rot = |rot: u32, other: u32| -> Option<(u32, u32, u32, u32)> {
                        let ri = to_idx(rot);
                        let rop = view.op(ri);
                        if at_u32(&use_count, ri) == 1
                            && at_u32(&inst_block, ri) == b
                            && matches!(rop, Op::Rotl | Op::Rotr)
                            && !at_bool(&skip_emit, ri)
                        {
                            let rat = to_idx(at_u32(view.data, ri));
                            let sb = at_u32(view.extra, rat);
                            let sk = at_u32(view.extra, rat + 1);
                            if let Some(k) = at_opt(&const_u64, to_idx(sk)) {
                                let mask = bits - 1;
                                let sh = trunc_u32(k) & mask;
                                let ror_amt = if rop == Op::Rotl { (bits - sh) & mask } else { sh };
                                return Some((other, sb, ror_amt, sk));
                            }
                        }
                        None
                    };
                    if let Some((a, sb, shift, sk)) = try_xor_rot(rhs, lhs) {
                        set_at(&mut fusion, inst, Fusion::EorRor { a, b: sb, shift, imm_inst: sk });
                        set_at(&mut skip_emit, to_idx(rhs), true);
                        continue;
                    }
                    if let Some((a, sb, shift, sk)) = try_xor_rot(lhs, rhs) {
                        set_at(&mut fusion, inst, Fusion::EorRor { a, b: sb, shift, imm_inst: sk });
                        set_at(&mut skip_emit, to_idx(lhs), true);
                        continue;
                    }
                }
                // Fuse `and`/`or`/`xor(a, shl/lshr/ashr(b, k))` (commutative, 64-bit or 32-bit, `1 <= k < bits`)
                // -> single ARM64 shifted-register `and`/`orr`/`eor rd, ra, rb, {lsl,lsr,asr} #k`!
                let try_log_shift = |sh: u32, other: u32| -> Option<(LogShift, u32, u32, u32, u32)> {
                    let si = to_idx(sh);
                    let kind = match view.op(si) {
                        Op::Shl => LogShift::Lsl,
                        Op::Lshr => LogShift::Lsr,
                        Op::Ashr => LogShift::Asr,
                        _ => return None,
                    };
                    if at_u32(&use_count, si) != 1
                        || at_u32(&inst_block, si) != b
                        || at_bool(&skip_emit, si)
                        || at_fusion(&fusion, si) != Fusion::None
                    {
                        return None;
                    }
                    // Keep already-immediate same-block single-use logical constants (`and`/`orr`/`eor #imm`)
                    // as immediates instead of forcing constant materialization + shifted register.
                    let oi = to_idx(other);
                    if at_u32(&use_count, oi) == 1
                        && at_u32(&inst_block, oi) == b
                        && at_opt(&const_u64, oi).is_some_and(|c| {
                            enc::logical_imm(c, bits).is_some()
                                || (op == Op::And && bits == 64 && is_low_bitmask(c).is_some())
                        })
                    {
                        return None;
                    }
                    let sat = to_idx(at_u32(view.data, si));
                    let sb = at_u32(view.extra, sat);
                    let sk = at_u32(view.extra, sat + 1);
                    let k = at_opt(&const_u64, to_idx(sk))?;
                    if k >= 1 && k < u64::from(bits) {
                        Some((kind, other, sb, trunc_u32(k), sk))
                    } else {
                        None
                    }
                };
                if let Some((kind, a, sb, shift, sk)) = try_log_shift(rhs, lhs) {
                    set_at(&mut fusion, inst, Fusion::LogicalShifted { a, b: sb, kind, shift, imm_inst: sk });
                    set_at(&mut skip_emit, to_idx(rhs), true);
                    continue;
                }
                if let Some((kind, a, sb, shift, sk)) = try_log_shift(lhs, rhs) {
                    set_at(&mut fusion, inst, Fusion::LogicalShifted { a, b: sb, kind, shift, imm_inst: sk });
                    set_at(&mut skip_emit, to_idx(lhs), true);
                    continue;
                }
            }
            Op::Load | Op::Store => {
                let at = to_idx(at_u32(view.data, inst));
                let (ptr, off, access_ty, store_val) = if op == Op::Load {
                    (at_u32(view.extra, at), i64::from(at_u32(view.extra, at + 1).cast_signed()), ty, u32::MAX)
                } else {
                    let val = at_u32(view.extra, at);
                    (at_u32(view.extra, at + 1), i64::from(at_u32(view.extra, at + 2).cast_signed()), ty_at(view.tys, to_idx(val)), val)
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
                if !slots_after_spills && pop == Op::SlotAddr {
                    let s_off = i64::from(at_u32(&slot_at, to_idx(at_u32(view.data, pi))));
                    let total = s_off + off;
                    if fits_off(total) {
                        set_at(&mut fusion, inst, Fusion::MemSpOff { off: total });
                        inc_u32(&mut slot_addr_folded_uses, pi);
                        continue;
                    }
                }
                // Pattern B0: Multi-use (1..=4 uses, even cross-block!) non-volatile offset-0
                // `ptr_add(base, shl(idx, scale))` (including shared `shl(idx, scale)` across
                // 1..=4 foldable `ptr_add`s like `keys[slot]` + `vals[slot]`) or byte
                // `ptr_add(base, idx)` -> `[base, idx, lsl #scale]`!
                let pi_uses = at_u32(&use_count, pi);
                let is_volatile = Flags(at_u8(view.flags, inst)).contains(Flags::VOLATILE);
                if !is_volatile
                    && store_val != ptr
                    && off == 0
                    && pop == Op::PtrAdd
                    && mem_sz.is_some()
                    && (1..=4).contains(&pi_uses)
                    && at_fusion(&fusion, pi) == Fusion::None
                {
                    let pat = to_idx(at_u32(view.data, pi));
                    let base = at_u32(view.extra, pat);
                    let by = at_u32(view.extra, pat + 1);
                    if ty_at(view.tys, to_idx(by)).bits(64) == 64 {
                        let bi = to_idx(by);
                        let bi_uses = at_u32(&use_count, bi);
                        if size_shift > 0
                            && at_u32(&ptr_shl_foldable_uses, pi) == pi_uses
                            && (1..=4).contains(&bi_uses)
                            && at_u32(&shl_foldable_ptr_uses, bi) == bi_uses
                            && view.op(bi) == Op::Shl
                            && at_fusion(&fusion, bi) == Fusion::None
                        {
                            let sat = to_idx(at_u32(view.data, bi));
                            let idx_v = at_u32(view.extra, sat);
                            let sh_v = at_u32(view.extra, sat + 1);
                            if at_opt(&const_u64, to_idx(sh_v)) == Some(u64::from(size_shift)) {
                                // Count `imm_uses[sh_v]` ONLY ONCE per folded `shl` even when
                                // shared across multiple `PtrAdd`s and multiple loads/stores!
                                let first_fold_shl = !at_bool(&skip_emit, bi);
                                let shift_imm = if first_fold_shl { sh_v } else { u32::MAX };
                                let zi = to_idx(idx_v);
                                if at_u32(&use_count, zi) == 1
                                    && view.op(zi) == Op::Zext
                                    && (!at_bool(&skip_emit, zi) || !first_fold_shl)
                                {
                                    let src_v = at_u32(view.data, zi);
                                    if ty_at(view.tys, to_idx(src_v)).bits(64) == 32 {
                                        set_at(
                                            &mut fusion,
                                            inst,
                                            Fusion::MemUxtwIdx {
                                                base,
                                                idx: src_v,
                                                scaled: true,
                                                shift_imm,
                                            },
                                        );
                                        set_at(&mut skip_emit, pi, true);
                                        set_at(&mut skip_emit, bi, true);
                                        set_at(&mut skip_emit, zi, true);
                                        continue;
                                    }
                                }
                                set_at(
                                    &mut fusion,
                                    inst,
                                    Fusion::MemScaledIdx { base, idx: idx_v, shift_imm },
                                );
                                set_at(&mut skip_emit, pi, true);
                                set_at(&mut skip_emit, bi, true);
                                continue;
                            }
                        } else if size_shift == 0
                            && at_u32(&ptr_byte_foldable_uses, pi) == pi_uses
                            && at_opt(&const_u64, bi).is_none()
                        {
                            let first_fold = !at_bool(&skip_emit, pi);
                            if bi_uses == 1
                                && view.op(bi) == Op::Shl
                                && (!at_bool(&skip_emit, bi) || !first_fold)
                            {
                                let sat = to_idx(at_u32(view.data, bi));
                                let idx_v = at_u32(view.extra, sat);
                                let sh_v = at_u32(view.extra, sat + 1);
                                if at_opt(&const_u64, to_idx(sh_v)) == Some(0) {
                                    let shift_imm = if first_fold { sh_v } else { u32::MAX };
                                    set_at(
                                        &mut fusion,
                                        inst,
                                        Fusion::MemScaledIdx { base, idx: idx_v, shift_imm },
                                    );
                                    set_at(&mut skip_emit, pi, true);
                                    set_at(&mut skip_emit, bi, true);
                                    continue;
                                }
                            }
                            if bi_uses == 1
                                && view.op(bi) == Op::Zext
                                && (!at_bool(&skip_emit, bi) || !first_fold)
                            {
                                let src_v = at_u32(view.data, bi);
                                if ty_at(view.tys, to_idx(src_v)).bits(64) == 32 {
                                    set_at(
                                        &mut fusion,
                                        inst,
                                        Fusion::MemUxtwIdx {
                                            base,
                                            idx: src_v,
                                            scaled: false,
                                            shift_imm: u32::MAX,
                                        },
                                    );
                                    set_at(&mut skip_emit, pi, true);
                                    set_at(&mut skip_emit, bi, true);
                                    continue;
                                }
                            }
                            set_at(&mut fusion, inst, Fusion::MemRegIdx { base, idx: by });
                            set_at(&mut skip_emit, pi, true);
                            continue;
                        }
                    }
                }
                // Pattern B: `load`/`store` on same-block `add`/`ptr_add` -> `[base, #imm]` or `[base, idx, lsl #size]`!
                let pi_same_blk = at_u32(&inst_block, pi) == b;
                let pi_all_fold_off = pi_uses > 1
                    && pi_same_blk
                    && at_u32(&ptr_off_foldable_uses, pi) == pi_uses
                    && at_fusion(&fusion, pi) == Fusion::None;
                if (pi_uses == 1 && pi_same_blk && !at_bool(&skip_emit, pi)) || pi_all_fold_off {
                    if pop == Op::PtrAdd {
                        let pat = to_idx(at_u32(view.data, pi));
                        let base = at_u32(view.extra, pat);
                        let by = at_u32(view.extra, pat + 1);
                        if let Some(cv) = at_opt(&const_u64, to_idx(by)) {
                            let by_bits = ty_at(view.tys, to_idx(by)).bits(64);
                            let delta = sign_extend_u64(cv, by_bits);
                            if let Some(total) = delta.checked_add(off) {
                                if fits_off(total) {
                                    let imm_inst = if at_bool(&skip_emit, pi) { u32::MAX } else { by };
                                    set_at(&mut fusion, inst, Fusion::MemBaseOff { base, off: total, imm_inst });
                                    set_at(&mut skip_emit, pi, true);
                                    continue;
                                }
                            }
                        } else if pi_uses == 1 && off == 0 && ty_at(view.tys, to_idx(by)).bits(64) == 64 {
                            // Pattern B2: `ptr_add(base, shl(idx, log2(size)))` -> `[base, idx, lsl #size]`
                            // (or `[base, widx, uxtw #size]` when `idx = zext(u32 -> u64, widx)`).
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
                                    let zi = to_idx(idx_v);
                                    if mem_sz.is_some()
                                        && at_u32(&use_count, zi) == 1
                                        && at_u32(&inst_block, zi) == b
                                        && view.op(zi) == Op::Zext
                                        && !at_bool(&skip_emit, zi)
                                    {
                                        let src_v = at_u32(view.data, zi);
                                        if ty_at(view.tys, to_idx(src_v)).bits(64) == 32 {
                                            set_at(
                                                &mut fusion,
                                                inst,
                                                Fusion::MemUxtwIdx {
                                                    base,
                                                    idx: src_v,
                                                    scaled: true,
                                                    shift_imm: sh_v,
                                                },
                                            );
                                            set_at(&mut skip_emit, pi, true);
                                            set_at(&mut skip_emit, bi, true);
                                            set_at(&mut skip_emit, zi, true);
                                            continue;
                                        }
                                    }
                                    set_at(&mut fusion, inst, Fusion::MemScaledIdx { base, idx: idx_v, shift_imm: sh_v });
                                    set_at(&mut skip_emit, pi, true);
                                    set_at(&mut skip_emit, bi, true);
                                    continue;
                                }
                            }
                            // Pattern B3: `ptr_add(base, zext(u32 -> u64, widx))` -> `[base, widx, uxtw]`.
                            if mem_sz.is_some()
                                && at_u32(&use_count, bi) == 1
                                && at_u32(&inst_block, bi) == b
                                && view.op(bi) == Op::Zext
                                && !at_bool(&skip_emit, bi)
                            {
                                let src_v = at_u32(view.data, bi);
                                if ty_at(view.tys, to_idx(src_v)).bits(64) == 32 {
                                    set_at(
                                        &mut fusion,
                                        inst,
                                        Fusion::MemUxtwIdx {
                                            base,
                                            idx: src_v,
                                            scaled: false,
                                            shift_imm: u32::MAX,
                                        },
                                    );
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
                        if pi_uses == 1
                            && let Fusion::AddLsl { a: base, b: idx, shift, imm_inst } = at_fusion(&fusion, pi)
                        {
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
                            if let Some((base, by, delta)) = cand {
                                if let Some(total) = delta.checked_add(off) {
                                    if fits_off(total) {
                                        let imm_inst = if at_bool(&skip_emit, pi) { u32::MAX } else { by };
                                        set_at(&mut fusion, inst, Fusion::MemBaseOff { base, off: total, imm_inst });
                                        set_at(&mut skip_emit, pi, true);
                                        continue;
                                    }
                                }
                            } else if pi_uses == 1 && off == 0 {
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
                            // Fuse `d = sub(a, rhs); icmp(Eq/Ne, d, 0); br` -> `subs rd, ra, rhs; b.eq/b.ne`
                            // (macro-op fused on Apple Silicon / Neoverse when adjacent, zero extra `cmp`/`cbnz`)!
                            if !fused_bit_br && matches!(cond, IntCond::Eq | IntCond::Ne) {
                                let sub_zero = if at_opt(&const_u64, to_idx(bv)) == Some(0) {
                                    Some((to_idx(a), bv))
                                } else if at_opt(&const_u64, to_idx(a)) == Some(0) {
                                    Some((to_idx(bv), a))
                                } else {
                                    None
                                };
                                if let Some((sub_i, zero_imm)) = sub_zero {
                                    let sub_ty = ty_at(view.tys, sub_i);
                                    let sub_bits = if sub_ty.is_scalar() && sub_ty.is_int() { sub_ty.bits(64) } else { 0 };
                                    if (sub_bits == 64 || sub_bits == 32)
                                        && at_u32(&inst_block, sub_i) == b
                                        && view.op(sub_i) == Op::Sub
                                        && !at_bool(&skip_emit, sub_i)
                                        && at_fusion(&fusion, sub_i) == Fusion::None
                                    {
                                        let mut flags_safe = true;
                                        for k in (sub_i + 1)..inst {
                                            if at_bool(&skip_emit, k) || k == cond_v {
                                                continue;
                                            }
                                            if !op_preserves_nzcv(view.op(k), ty_at(view.tys, k), at_fusion(&fusion, k)) {
                                                flags_safe = false;
                                                break;
                                            }
                                        }
                                        if flags_safe {
                                            set_at(&mut fusion, sub_i, Fusion::SubsFlags);
                                            set_at(&mut fusion, inst, Fusion::ReuseFlagsBr { cond: cond_of(cond), zero_imm });
                                            set_at(&mut skip_emit, cond_v, true);
                                            fused_bit_br = true;
                                        }
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
                                    let add1 = if at_opt(&const_u64, to_idx(fr)) == Some(1) {
                                        Some((fl, fr))
                                    } else if at_opt(&const_u64, to_idx(fl)) == Some(1) {
                                        Some((fr, fl))
                                    } else {
                                        None
                                    };
                                    if let Some((rm, one_imm)) = add1 {
                                        set_at(&mut fusion, inst, Fusion::CsincSelect {
                                            cond,
                                            a,
                                            b: bv,
                                            op_ty,
                                            rn: t_val,
                                            rm,
                                            inc_when_false: true,
                                            one_imm,
                                        });
                                        set_at(&mut skip_emit, cond_v, true);
                                        set_at(&mut skip_emit, fi, true);
                                        special = true;
                                    }
                                } else if at_u32(&use_count, ti) == 1 && at_u32(&inst_block, ti) == b && view.op(ti) == Op::Add && at_fusion(&fusion, ti) == Fusion::None && !at_bool(&skip_emit, ti) {
                                    let tat = to_idx(at_u32(view.data, ti));
                                    let (tl, tr) = (at_u32(view.extra, tat), at_u32(view.extra, tat + 1));
                                    let add1 = if at_opt(&const_u64, to_idx(tr)) == Some(1) {
                                        Some((tl, tr))
                                    } else if at_opt(&const_u64, to_idx(tl)) == Some(1) {
                                        Some((tr, tl))
                                    } else {
                                        None
                                    };
                                    if let Some((rm, one_imm)) = add1 {
                                        set_at(&mut fusion, inst, Fusion::CsincSelect {
                                            cond,
                                            a,
                                            b: bv,
                                            op_ty,
                                            rn: f_val,
                                            rm,
                                            inc_when_false: false,
                                            one_imm,
                                        });
                                        set_at(&mut skip_emit, cond_v, true);
                                        set_at(&mut skip_emit, ti, true);
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
            Op::Mul if ty == Ty::I128 => {
                let at = to_idx(at_u32(view.data, inst));
                let lhs = at_u32(view.extra, at);
                let rhs = at_u32(view.extra, at + 1);
                let try_zext_src = |v: u32, other: u32| -> Option<u32> {
                    if v == other {
                        return None;
                    }
                    let vi = to_idx(v);
                    if at_u32(&use_count, vi) == 1
                        && at_u32(&inst_block, vi) == b
                        && view.op(vi) == Op::Zext
                        && !at_bool(&skip_emit, vi)
                        && at_fusion(&fusion, vi) == Fusion::None
                    {
                        let src_v = at_u32(view.data, vi);
                        let src_ty = ty_at(view.tys, to_idx(src_v));
                        if src_ty.is_scalar() && src_ty.is_int() && src_ty.bits(64) <= 64 {
                            return Some(src_v);
                        }
                    }
                    None
                };
                let lhs_src = try_zext_src(lhs, rhs);
                let rhs_src = try_zext_src(rhs, lhs);
                if lhs_src.is_some() || rhs_src.is_some() {
                    set_at(&mut fusion, inst, Fusion::I128WideMul { lhs_src, rhs_src });
                    if lhs_src.is_some() {
                        set_at(&mut skip_emit, to_idx(lhs), true);
                    }
                    if rhs_src.is_some() {
                        set_at(&mut skip_emit, to_idx(rhs), true);
                    }
                }
            }
            Op::Trunc if bits > 0 => {
                let src_v = at_u32(view.data, inst);
                let si = to_idx(src_v);
                if ty_at(view.tys, si) == Ty::I128
                    && at_u32(&use_count, si) == 1
                    && at_u32(&inst_block, si) == b
                    && matches!(view.op(si), Op::Lshr | Op::Ashr)
                    && !at_bool(&skip_emit, si)
                    && at_fusion(&fusion, si) == Fusion::None
                {
                    let sat = to_idx(at_u32(view.data, si));
                    let x = at_u32(view.extra, sat);
                    let bv = at_u32(view.extra, sat + 1);
                    let bvi = to_idx(bv);
                    let const_sh: Option<u32> = if let Some(c) = at_opt(&const_u64, bvi) {
                        Some(trunc_u32(c & 127))
                    } else if view.op(bvi) == Op::Iconst {
                        Some(at_u32(view.data, bvi) & 127)
                    } else if view.op(bvi) == Op::IconstWide {
                        Some(at_u32(view.extra, to_idx(at_u32(view.data, bvi))) & 127)
                    } else {
                        None
                    };
                    if const_sh == Some(64) {
                        set_at(&mut fusion, inst, Fusion::I128HiTrunc { src: x, shift_imm: bv });
                        set_at(&mut skip_emit, si, true);
                    }
                } else if bits <= 64
                    && ty_at(view.tys, si) == Ty::I128
                    && at_u32(&use_count, si) == 1
                    && at_u32(&inst_block, si) == b
                    && view.op(si) == Op::Mul
                    && !at_bool(&skip_emit, si)
                {
                    let mat = to_idx(at_u32(view.data, si));
                    let lhs = at_u32(view.extra, mat);
                    let rhs = at_u32(view.extra, mat + 1);
                    match at_fusion(&fusion, si) {
                        Fusion::None => {
                            set_at(&mut fusion, inst, Fusion::I128LoMul { lhs, rhs, lhs_src: None, rhs_src: None });
                            set_at(&mut skip_emit, si, true);
                        }
                        Fusion::I128WideMul { lhs_src, rhs_src } => {
                            set_at(&mut fusion, inst, Fusion::I128LoMul { lhs, rhs, lhs_src, rhs_src });
                            set_at(&mut skip_emit, si, true);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    // 2a-ii. Multi-use shared `Icmp`/`Fcmp` fusion (`bsearch` dual-`Select` -> ONE `cmp` + TWO `csel`/`csinc`!):
    // When a compare has >= 2 uses, ALL in the same block as live `Select`/`Br` condition operands
    // with no intervening NZCV-clobbering instruction between the first and last user, emit `cmp`
    // once at the first user and have every subsequent user reuse live hardware NZCV flags directly!
    {
        let mut shared_users: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(8, alloc);
        for cond_v in 0..len {
            if at_bool(&skip_emit, cond_v) || at_u32(&use_count, cond_v) < 2 {
                continue;
            }
            let cop = view.op(cond_v);
            let cmp_info = if cop == Op::Icmp && is_vec_mask_ty(ty_at(view.tys, cond_v)).is_none() {
                let cat = to_idx(at_u32(view.data, cond_v));
                IntCond::from_word(at_u32(view.extra, cat)).and_then(|ic| {
                    let a = at_u32(view.extra, cat + 1);
                    let bv = at_u32(view.extra, cat + 2);
                    let op_ty = ty_at(view.tys, to_idx(a));
                    if op_ty == Ty::I128 {
                        None
                    } else {
                        Some((cond_of(ic), Some((ic, a, bv, op_ty)), None))
                    }
                })
            } else if cop == Op::Fcmp {
                let cat = to_idx(at_u32(view.data, cond_v));
                FloatCond::from_word(at_u32(view.extra, cat)).and_then(fcond_of).and_then(|fc| {
                    let a = at_u32(view.extra, cat + 1);
                    let bv = at_u32(view.extra, cat + 2);
                    is_fp_ty(ty_at(view.tys, to_idx(a))).map(|double| (fc, None, Some((fc, a, bv, double))))
                })
            } else {
                None
            };
            let Some((hw_cond, int_cmp, fp_cmp)) = cmp_info else {
                continue;
            };
            let b = at_u32(&inst_block, cond_v);
            let range = view.block(Block(b));
            shared_users.clear();
            let mut matched_uses = 0u32;
            let mut all_ok = true;
            for k in (cond_v + 1)..range.end {
                let mut uses_in_k = 0u32;
                regalloc::for_each_use(&view, k, |v| {
                    if to_idx(v) == cond_v {
                        uses_in_k += 1;
                    }
                });
                if uses_in_k == 0 {
                    continue;
                }
                if uses_in_k > 1 || at_bool(&skip_emit, k) || at_fusion(&fusion, k) != Fusion::None {
                    all_ok = false;
                    break;
                }
                let kop = view.op(k);
                let kat = to_idx(at_u32(view.data, k));
                if kop == Op::Select {
                    let kty = ty_at(view.tys, k);
                    if to_idx(at_u32(view.extra, kat)) == cond_v
                        && is_vec128_ty(kty).is_none()
                        && kty != Ty::I128
                        && at_u32(&use_count, k) > 0
                    {
                        shared_users.push(k);
                        matched_uses += 1;
                    } else {
                        all_ok = false;
                        break;
                    }
                } else if kop == Op::Br && to_idx(at_u32(view.extra, kat)) == cond_v {
                    shared_users.push(k);
                    matched_uses += 1;
                } else {
                    all_ok = false;
                    break;
                }
            }
            if !all_ok || matched_uses != at_u32(&use_count, cond_v) || shared_users.len() < 2 {
                continue;
            }
            let (Some(&first_u), Some(&last_u)) = (shared_users.first(), shared_users.last()) else {
                continue;
            };
            if view.op(first_u) != Op::Select {
                continue;
            }
            // Verify every instruction strictly between `first_u` and `last_u` is either another
            // user of `cond_v` or strictly guaranteed to preserve ARM64 NZCV flags!
            let mut flags_safe = true;
            for k in (first_u + 1)..last_u {
                if at_bool(&skip_emit, k) || shared_users.iter().any(|&u| u == k) {
                    continue;
                }
                if !op_preserves_nzcv(view.op(k), ty_at(view.tys, k), at_fusion(&fusion, k)) {
                    flags_safe = false;
                    break;
                }
            }
            if !flags_safe {
                continue;
            }
            // Commit shared-flag fusion: emit `cmp`/`fcmp` once at `first_u`, reuse NZCV on all others!
            set_at(&mut skip_emit, cond_v, true);
            let try_add_one = |val: u32, skip_ref: &[bool], fus_ref: &[Fusion]| -> Option<(usize, u32, u32)> {
                let vi = to_idx(val);
                if at_u32(&use_count, vi) == 1
                    && at_u32(&inst_block, vi) == b
                    && view.op(vi) == Op::Add
                    && at_fusion(fus_ref, vi) == Fusion::None
                    && !at_bool(skip_ref, vi)
                {
                    let vat = to_idx(at_u32(view.data, vi));
                    let (vl, vr) = (at_u32(view.extra, vat), at_u32(view.extra, vat + 1));
                    if at_opt(&const_u64, to_idx(vr)) == Some(1) {
                        return Some((vi, vl, vr));
                    }
                    if at_opt(&const_u64, to_idx(vl)) == Some(1) {
                        return Some((vi, vr, vl));
                    }
                }
                None
            };
            for (u_idx, &u) in shared_users.iter().enumerate() {
                if view.op(u) == Op::Br {
                    set_at(&mut fusion, u, Fusion::ReuseFlagsBr { cond: hw_cond, zero_imm: u32::MAX });
                    continue;
                }
                let uat = to_idx(at_u32(view.data, u));
                let t_val = at_u32(view.extra, uat + 1);
                let f_val = at_u32(view.extra, uat + 2);
                let uty = ty_at(view.tys, u);
                let is_i64_sel = uty.is_scalar() && uty.is_int() && uty.bits(64) == 64;
                if u_idx == 0 {
                    if let Some((ic, a, bv, op_ty)) = int_cmp {
                        if is_i64_sel {
                            if let Some((fi, rm, one_imm)) = try_add_one(f_val, &skip_emit, &fusion) {
                                set_at(&mut fusion, u, Fusion::CsincSelect {
                                    cond: ic,
                                    a,
                                    b: bv,
                                    op_ty,
                                    rn: t_val,
                                    rm,
                                    inc_when_false: true,
                                    one_imm,
                                });
                                set_at(&mut skip_emit, fi, true);
                                continue;
                            }
                            if let Some((ti, rm, one_imm)) = try_add_one(t_val, &skip_emit, &fusion) {
                                set_at(&mut fusion, u, Fusion::CsincSelect {
                                    cond: ic,
                                    a,
                                    b: bv,
                                    op_ty,
                                    rn: f_val,
                                    rm,
                                    inc_when_false: false,
                                    one_imm,
                                });
                                set_at(&mut skip_emit, ti, true);
                                continue;
                            }
                        }
                        set_at(&mut fusion, u, Fusion::CmpSelect { cond: ic, a, b: bv, op_ty });
                    } else if let Some((fc, a, bv, double)) = fp_cmp {
                        set_at(&mut fusion, u, Fusion::FcmpSelect { cond: fc, a, b: bv, double });
                    }
                } else {
                    if is_i64_sel {
                        if let Some((fi, rm, one_imm)) = try_add_one(f_val, &skip_emit, &fusion) {
                            set_at(&mut fusion, u, Fusion::ReuseFlagsCsinc { cond: hw_cond, rn: t_val, rm, one_imm });
                            set_at(&mut skip_emit, fi, true);
                            continue;
                        }
                        if let Some((ti, rm, one_imm)) = try_add_one(t_val, &skip_emit, &fusion) {
                            set_at(&mut fusion, u, Fusion::ReuseFlagsCsinc {
                                cond: invert_cond(hw_cond),
                                rn: f_val,
                                rm,
                                one_imm,
                            });
                            set_at(&mut skip_emit, ti, true);
                            continue;
                        }
                    }
                    set_at(&mut fusion, u, Fusion::ReuseFlagsSelect { cond: hw_cond });
                }
            }
        }
    }

    // 2a-iii. Multi-use FP `Op::Splat` -> by-element NEON `fmul`/`fmla`/`fmls` fusion:
    // Eliminates all `dup.4s`/`dup.2d` instructions in `matmul_f32`/`matmul_f64`/`saxpy` whenever
    // every surviving use of an FP vector `Op::Splat(scalar)` is a multiplicand of `Fmul`/`Fma`/
    // `Fmuladd`/`Fmadd`/`Fmsub`.
    let has_fp_splat = (0..len).any(|i| {
        !at_bool(&skip_emit, i)
            && view.op(i) == Op::Splat
            && at_u32(&use_count, i) > 0
            && is_vec128_ty(ty_at(view.tys, i)).is_some_and(|(_, vfp)| vfp)
    });
    if has_fp_splat {
        let mut splat_fusable_uses: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
        let mut splat_disqualified: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
        let mut splat_fused: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
        for _ in 0..len {
            splat_fusable_uses.push(0);
            splat_disqualified.push(false);
            splat_fused.push(false);
        }
        let is_fp_splat_cand = |v: u32, skip_ref: &[bool]| -> bool {
            let vi = to_idx(v);
            vi < len
                && !at_bool(skip_ref, vi)
                && view.op(vi) == Op::Splat
                && at_u32(&use_count, vi) > 0
                && is_vec128_ty(ty_at(view.tys, vi)).is_some_and(|(_, vfp)| vfp)
        };
        for u in 0..len {
            if at_bool(&skip_emit, u) {
                continue;
            }
            if !is_vec128_ty(ty_at(view.tys, u)).is_some_and(|(_, vfp)| vfp) {
                continue;
            }
            let uop = view.op(u);
            let ufus = at_fusion(&fusion, u);
            let operands = match (uop, ufus) {
                (Op::Fmul, Fusion::None) => {
                    let uat = to_idx(at_u32(view.data, u));
                    Some((at_u32(view.extra, uat), at_u32(view.extra, uat + 1), None))
                }
                (Op::Fma | Op::Fmuladd, Fusion::None) => {
                    let uat = to_idx(at_u32(view.data, u));
                    Some((at_u32(view.extra, uat), at_u32(view.extra, uat + 1), Some(at_u32(view.extra, uat + 2))))
                }
                (Op::Fadd, Fusion::Fmadd { a, b, acc }) | (Op::Fsub, Fusion::Fmsub { a, b, acc }) => {
                    Some((a, b, Some(acc)))
                }
                _ => None,
            };
            if let Some((ma, mb, acc_opt)) = operands {
                if let Some(acc) = acc_opt {
                    if is_fp_splat_cand(acc, &skip_emit) {
                        set_at(&mut splat_disqualified, to_idx(acc), true);
                    }
                }
                let a_cand = is_fp_splat_cand(ma, &skip_emit);
                let b_cand = is_fp_splat_cand(mb, &skip_emit);
                if ma == mb {
                    if a_cand {
                        set_at(&mut splat_disqualified, to_idx(ma), true);
                    }
                } else if a_cand && b_cand {
                    set_at(&mut splat_disqualified, to_idx(ma), true);
                    inc_u32(&mut splat_fusable_uses, to_idx(mb));
                } else if a_cand {
                    inc_u32(&mut splat_fusable_uses, to_idx(ma));
                } else if b_cand {
                    inc_u32(&mut splat_fusable_uses, to_idx(mb));
                }
            }
        }
        let mut any_fused = false;
        for i in 0..len {
            if is_fp_splat_cand(u32_of(i), &skip_emit)
                && !at_bool(&splat_disqualified, i)
                && at_u32(&splat_fusable_uses, i) == at_u32(&use_count, i)
            {
                set_at(&mut splat_fused, i, true);
                any_fused = true;
            }
        }
        if any_fused {
            for i in 0..len {
                if at_bool(&splat_fused, i) {
                    set_at(&mut skip_emit, i, true);
                }
            }
            for u in 0..len {
                if at_bool(&skip_emit, u) {
                    continue;
                }
                if !is_vec128_ty(ty_at(view.tys, u)).is_some_and(|(_, vfp)| vfp) {
                    continue;
                }
                let uop = view.op(u);
                let ufus = at_fusion(&fusion, u);
                match (uop, ufus) {
                    (Op::Fmul, Fusion::None) => {
                        let uat = to_idx(at_u32(view.data, u));
                        let (ma, mb) = (at_u32(view.extra, uat), at_u32(view.extra, uat + 1));
                        if at_bool(&splat_fused, to_idx(mb)) {
                            let scalar = at_u32(view.data, to_idx(mb));
                            set_at(&mut fusion, u, Fusion::VecFmulElem { vec: ma, scalar });
                        } else if at_bool(&splat_fused, to_idx(ma)) {
                            let scalar = at_u32(view.data, to_idx(ma));
                            set_at(&mut fusion, u, Fusion::VecFmulElem { vec: mb, scalar });
                        }
                    }
                    (Op::Fma | Op::Fmuladd, Fusion::None) => {
                        let uat = to_idx(at_u32(view.data, u));
                        let (ma, mb, acc) = (
                            at_u32(view.extra, uat),
                            at_u32(view.extra, uat + 1),
                            at_u32(view.extra, uat + 2),
                        );
                        if at_bool(&splat_fused, to_idx(mb)) {
                            let scalar = at_u32(view.data, to_idx(mb));
                            set_at(&mut fusion, u, Fusion::VecFmaddElem { vec: ma, scalar, acc });
                        } else if at_bool(&splat_fused, to_idx(ma)) {
                            let scalar = at_u32(view.data, to_idx(ma));
                            set_at(&mut fusion, u, Fusion::VecFmaddElem { vec: mb, scalar, acc });
                        }
                    }
                    (Op::Fadd, Fusion::Fmadd { a: ma, b: mb, acc }) => {
                        if at_bool(&splat_fused, to_idx(mb)) {
                            let scalar = at_u32(view.data, to_idx(mb));
                            set_at(&mut fusion, u, Fusion::VecFmaddElem { vec: ma, scalar, acc });
                        } else if at_bool(&splat_fused, to_idx(ma)) {
                            let scalar = at_u32(view.data, to_idx(ma));
                            set_at(&mut fusion, u, Fusion::VecFmaddElem { vec: mb, scalar, acc });
                        }
                    }
                    (Op::Fsub, Fusion::Fmsub { a: ma, b: mb, acc }) => {
                        if at_bool(&splat_fused, to_idx(mb)) {
                            let scalar = at_u32(view.data, to_idx(mb));
                            set_at(&mut fusion, u, Fusion::VecFmsubElem { vec: ma, scalar, acc });
                        } else if at_bool(&splat_fused, to_idx(ma)) {
                            let scalar = at_u32(view.data, to_idx(ma));
                            set_at(&mut fusion, u, Fusion::VecFmsubElem { vec: mb, scalar, acc });
                        }
                    }
                    _ => {}
                }
            }
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
            | Fusion::LogicalShifted { imm_inst, .. }
            | Fusion::EorRor { imm_inst, .. } => {
                inc_u32(&mut imm_uses, to_idx(imm_inst));
            }
            Fusion::MemBaseOff { imm_inst, .. } => {
                if imm_inst != u32::MAX {
                    inc_u32(&mut imm_uses, to_idx(imm_inst));
                }
            }
            Fusion::RorImm { imm1, imm2, .. } => {
                inc_u32(&mut imm_uses, to_idx(imm1));
                inc_u32(&mut imm_uses, to_idx(imm2));
            }
            Fusion::Ubfx { shift_imm, mask_imm, .. } => {
                inc_u32(&mut imm_uses, to_idx(shift_imm));
                inc_u32(&mut imm_uses, to_idx(mask_imm));
            }
            Fusion::Bfxil { dst_mask_imm, src_mask_imm, shift_imm, .. } => {
                inc_u32(&mut imm_uses, to_idx(dst_mask_imm));
                inc_u32(&mut imm_uses, to_idx(src_mask_imm));
                if shift_imm != u32::MAX {
                    inc_u32(&mut imm_uses, to_idx(shift_imm));
                }
            }
            Fusion::MemScaledIdx { shift_imm, .. } => {
                if shift_imm != u32::MAX {
                    inc_u32(&mut imm_uses, to_idx(shift_imm));
                }
            }
            Fusion::MemUxtwIdx { shift_imm, .. } => {
                if shift_imm != u32::MAX {
                    inc_u32(&mut imm_uses, to_idx(shift_imm));
                }
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
            | Fusion::CsinvSelect { b: bv, op_ty, .. } => {
                if (op_ty.bits(64) == 64 || op_ty.bits(64) == 32)
                    && at_opt(&const_u64, to_idx(bv)).is_some_and(|v| v <= 4095)
                {
                    inc_u32(&mut imm_uses, to_idx(bv));
                }
            }
            Fusion::CsincSelect { b: bv, op_ty, one_imm, .. } => {
                if (op_ty.bits(64) == 64 || op_ty.bits(64) == 32)
                    && at_opt(&const_u64, to_idx(bv)).is_some_and(|v| v <= 4095)
                {
                    inc_u32(&mut imm_uses, to_idx(bv));
                }
                inc_u32(&mut imm_uses, to_idx(one_imm));
            }
            Fusion::ReuseFlagsCsinc { one_imm, .. } => {
                inc_u32(&mut imm_uses, to_idx(one_imm));
            }
            Fusion::ReuseFlagsBr { zero_imm, .. } => {
                if zero_imm != u32::MAX {
                    inc_u32(&mut imm_uses, to_idx(zero_imm));
                }
            }
            Fusion::SubsFlags => {
                let at = to_idx(at_u32(view.data, inst));
                let rhs = to_idx(at_u32(view.extra, at + 1));
                if at_opt(&const_u64, rhs).is_some_and(|v| v <= 4095) {
                    inc_u32(&mut imm_uses, rhs);
                }
            }
            Fusion::I128HiTrunc { shift_imm, .. } => {
                inc_u32(&mut imm_uses, to_idx(shift_imm));
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
            | Fusion::ReuseFlagsSelect { .. }
            | Fusion::MemSpOff { .. }
            | Fusion::MemRegIdx { .. }
            | Fusion::Bic { .. }
            | Fusion::Orn { .. }
            | Fusion::Eon { .. }
            | Fusion::TstBr { imm_nrs: None, .. }
            | Fusion::BlazeResultCallBr
            | Fusion::VecUadalp { .. }
            | Fusion::VecFmulElem { .. }
            | Fusion::VecFmaddElem { .. }
            | Fusion::VecFmsubElem { .. }
            | Fusion::I128WideMul { .. }
            | Fusion::I128LoMul { .. } => {}
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
                Op::Memmove => {
                    let at = to_idx(at_u32(view.data, inst));
                    let len_v = to_idx(at_u32(view.extra, at + 2));
                    if at_opt(&const_u64, len_v).is_some_and(|n| n <= SOTA_MEMMOVE_INLINE_MOST) {
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
        let is_foldable_const = at_opt(&const_u64, inst).is_some()
            || (ty_at(view.tys, inst) == Ty::I128 && matches!(view.op(inst), Op::Iconst | Op::IconstWide));
        if is_foldable_const && at_u32(&use_count, inst) > 0 && at_u32(&imm_uses, inst) == at_u32(&use_count, inst) {
            set_at(&mut skip_emit, inst, true);
        }
    }

    // Inline small constant `Op::Memmove` (`<= 32 B`) uses only reserved scratch (`X16`/`X17`/`V30`/`V31`)
    // with zero call instructions and zero caller-saved clobbers; exclude those sites from regalloc's
    // call-clobber / `has_calls` tracking while preserving `tail_calls` for `Op::Call`.
    let mut regalloc_no_call: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    for inst in 0..len {
        let no_call = at_bool(&tail_calls, inst) || small_const_memmove_len(&view, inst, &const_u64).is_some();
        regalloc_no_call.push(no_call);
    }

    // 3. IPRA-aware Greedy Priority-Weighted Graph-Coloring Regalloc + True Constant Rematerialization!
    let reg_alloc = regalloc::allocate_with_ipra_and_order(
        &view,
        &skip_emit,
        &regalloc_no_call,
        max_spill_slots,
        os,
        call_clobbers,
        scratch_order,
        alloc,
    )
    .map_err(|_| TierError::Refused {
        tier: TIER,
        inst: None,
        what: "spill slots exceeded Sota fast frame budget",
    })?;

    let spills_end = u64::from(spill_base) + u64::from(reg_alloc.num_spill_slots) * 8;
    let used_bytes = if slots_after_spills {
        let slots_base = spills_end.next_multiple_of(most_align);
        let total = slots_base.saturating_add(slots_end_rel);
        if total >= FRAME_LIMIT {
            return Err(TierError::Refused { tier: TIER, inst: None, what: "stack frame >= 16 MiB" });
        }
        let delta = u32_of(slots_base);
        for off in slot_at.iter_mut() {
            *off = off.wrapping_add(delta);
        }
        total
    } else {
        spills_end
    };
    let frame_size_u64 = used_bytes.next_multiple_of(16);
    if frame_size_u64 + (most_align - 16) >= FRAME_LIMIT {
        return Err(TierError::Refused { tier: TIER, inst: None, what: "stack frame >= 16 MiB" });
    }
    let frame_size = u32_of(frame_size_u64);
    let internal_err: core::cell::Cell<Option<TierError>> = core::cell::Cell::new(None);
    let mut data_relro_off: Vec<Option<u32>, Alloc<'a>> = Vec::with_capacity_in(view.datas.len(), alloc);
    for _ in 0..view.datas.len() {
        data_relro_off.push(None);
    }
    let mut out = Emitted::new_in(alloc);

    // Pre-resolve every rematerialized `Op::DataAddr` / `Op::FuncAddr` once so cold remat sites
    // only emit `adrp` + `add`/`ldr` and push their relocations without touching `names` or `data_relro_off`.
    let mut remat_global_plan: Vec<RematGlobalPlan, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    for _ in 0..len {
        remat_global_plan.push(RematGlobalPlan::None);
    }
    for g_inst in 0..len {
        if let AllocLoc::RematGlobal(_) = at_loc(&reg_alloc.loc_of, g_inst) {
            let plan = match view.op(g_inst) {
                Op::DataAddr => {
                    let rec = view.reference(RefId(at_u32(view.data, g_inst)));
                    match RefKind::from_word(rec.kind) {
                        Some(RefKind::Static) => {
                            let target = resolve_outside(names, Outside::Static(rec.place), Space::Static, true, &mut out, g_inst)?;
                            RematGlobalPlan::Got { target }
                        }
                        Some(RefKind::Vtable) => {
                            let target = resolve_outside(names, Outside::Vtable(rec.place), Space::Vtable, false, &mut out, g_inst)?;
                            RematGlobalPlan::Got { target }
                        }
                        Some(RefKind::Data) => {
                            let off = place_data_row(rec.place, &view, names, &mut data_relro_off, &mut out, g_inst, alloc)?;
                            RematGlobalPlan::OwnRelro { off }
                        }
                        _ => {
                            return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(g_inst)), what: "unsupported remat data_addr" });
                        }
                    }
                }
                Op::FuncAddr => {
                    let rec = view.reference(RefId(at_u32(view.data, g_inst)));
                    let target = resolve_outside(names, Outside::Func(rec.place), Space::Func, true, &mut out, g_inst)?;
                    RematGlobalPlan::Got { target }
                }
                _ => {
                    return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(g_inst)), what: "unexpected remat global op" });
                }
            };
            set_at(&mut remat_global_plan, g_inst, plan);
        }
    }
    let remat_relocs: core::cell::RefCell<Vec<Reloc, Alloc<'a>>> = core::cell::RefCell::new(Vec::new_in(alloc));
    let emit_remat_global = |words: &mut Vec<u32, Alloc<'a>>, rd: Reg, g_inst: u32| {
        match remat_global_plan.get(to_idx(g_inst)).copied().unwrap_or(RematGlobalPlan::None) {
            RematGlobalPlan::Got { target } => {
                let adrp_at = u32_of(words.len() * 4);
                words.push(enc::adrp(rd, 0));
                words.push(enc::ldr(Size::X, rd, rd, 0));
                let mut rels = remat_relocs.borrow_mut();
                rels.push(Reloc { at: adrp_at, in_relro: false, kind: RelocKind::GotPage21, target, addend: 0 });
                rels.push(Reloc { at: adrp_at + 4, in_relro: false, kind: RelocKind::GotPageOff12, target, addend: 0 });
            }
            RematGlobalPlan::OwnRelro { off } => {
                let adrp_at = u32_of(words.len() * 4);
                words.push(enc::adrp(rd, 0));
                words.push(enc::add_imm(rd, rd, 0, false));
                let mut rels = remat_relocs.borrow_mut();
                rels.push(Reloc { at: adrp_at, in_relro: false, kind: RelocKind::Page21, target: OWN_RELRO, addend: i64::from(off) });
                rels.push(Reloc { at: adrp_at + 4, in_relro: false, kind: RelocKind::PageOff12 { scale: 0 }, target: OWN_RELRO, addend: i64::from(off) });
            }
            RematGlobalPlan::None => {
                internal_err.set(Some(TierError::Refused { tier: TIER, inst: Some(g_inst), what: "missing remat global plan" }));
            }
        }
    };
    let emit_remat_write_reg = |words: &mut Vec<u32, Alloc<'a>>, rd: Reg, w: RematWrite| {
        match w {
            RematWrite::Const(cval) => emit_const_u64(words, rd, cval),
            RematWrite::Global(g_inst) => emit_remat_global(words, rd, g_inst),
        }
    };

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
            AllocLoc::RematGlobal(g_inst) => {
                emit_remat_global(words, scratch, g_inst);
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
    let mut const_edge_writes: Vec<(GprCopyLoc, RematWrite), Alloc<'a>> = Vec::with_capacity_in(8, alloc);

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
    let has_sme2_int_call = (0..len).any(|i| {
        if view.op(i) != Op::TargetIntrinsic {
            return false;
        }
        let at = to_idx(at_u32(view.data, i));
        let name = view.intrinsic_name(IntrinsicId(at_u32(view.extra, at)));
        name == b"blaze.sme2.gemm_i8_i32" || name == b"blaze.sme2.gemm_u8_u32"
    });
    let raw_clobbers = if has_sme2_int_call {
        reg_alloc.clobbers.clobbered.union(CallClobbers::C_ABI_ARM64.clobbered)
    } else {
        reg_alloc.clobbers.clobbered
    };
    let mut out_clobbers = CallClobbers { clobbered: raw_clobbers };
    let mut cold_gpr_pairs: Vec<(Reg, Reg), Alloc<'a>> = Vec::new_in(alloc);
    let mut cold_fp_pairs: Vec<(Reg, Reg), Alloc<'a>> = Vec::new_in(alloc);

    if preserve_all_cold {
        let sig_rec = view.sig(SigId(view.head.sig));
        let is_blaze_res = view
            .sigs
            .get(SigId(view.head.sig).index())
            .and_then(|r| Conv::from_byte(r.conv))
            == Some(Conv::BlazeResult);
        let mut ret_regs = RegSet::EMPTY;
        if sig_rec.rets.len() == 1 {
            if let Some(first_ret) = sig_rec.rets.first() {
                let r0 = Ty(first_ret.ty);
                if is_fp_ty(r0).is_some() || is_vec128_ty(r0).is_some() {
                    ret_regs = ret_regs.with(TierReg(32));
                } else if is_gpr_pair_ty(&view, r0) {
                    ret_regs = ret_regs.with(TierReg(0)).with(TierReg(1));
                } else {
                    ret_regs = ret_regs.with(TierReg(0));
                }
            }
        } else if sig_rec.rets.len() >= 2 {
            ret_regs = ret_regs.with(TierReg(0));
            if !is_blaze_res {
                ret_regs = ret_regs.with(TierReg(1));
            }
        }

        let has_any_call = reg_alloc.has_calls || has_sme2_int_call;
        let mut save_gprs: Vec<Reg, Alloc<'a>> = Vec::with_capacity_in(16, alloc);
        for r in 0..=15u8 {
            let tr = TierReg(r);
            if ret_regs.contains(tr) {
                continue;
            }
            if raw_clobbers.contains(tr) || (has_any_call && r <= 8) {
                save_gprs.push(u32::from(r));
                out_clobbers.clobbered = out_clobbers.clobbered.without(tr);
            }
        }
        let mut gi = 0usize;
        while gi < save_gprs.len() {
            let r0 = save_gprs.get(gi).copied().unwrap_or(X16);
            let r1 = save_gprs.get(gi + 1).copied().unwrap_or(X16);
            cold_gpr_pairs.push((r0, r1));
            gi += 2;
        }

        let mut save_fps: Vec<Reg, Alloc<'a>> = Vec::with_capacity_in(21, alloc);
        for v in 0..=28u8 {
            if (8..=15).contains(&v) {
                continue;
            }
            let tr = TierReg(32 + v);
            if ret_regs.contains(tr) {
                continue;
            }
            if raw_clobbers.contains(tr) || (has_any_call && v <= 7) {
                save_fps.push(u32::from(v));
                out_clobbers.clobbered = out_clobbers.clobbered.without(tr);
            }
        }
        let mut fi = 0usize;
        while fi < save_fps.len() {
            let v0 = save_fps.get(fi).copied().unwrap_or(V31);
            let v1 = save_fps.get(fi + 1).copied().unwrap_or(V31);
            cold_fp_pairs.push((v0, v1));
            fi += 2;
        }
    }

    let x_pairs = reg_alloc.x_pairs;
    let d_pairs = reg_alloc.d_pairs;
    let cs_bytes = (x_pairs.count_ones() + d_pairs.count_ones()) * 16;
    let cold_gpr_bytes = u32_of(cold_gpr_pairs.len()) * 16;
    let cold_fp_bytes = u32_of(cold_fp_pairs.len()) * 32;
    let total_saved_bytes = cs_bytes + cold_gpr_bytes + cold_fp_bytes;
    let needs_frame = reg_alloc.has_calls
        || has_sme2_int_call
        || x_pairs != 0
        || d_pairs != 0
        || cold_gpr_bytes != 0
        || cold_fp_bytes != 0
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

        // ColdPreserveAll: save modified caller-saved GPRs and 128-bit FP/NEON registers below
        // canonical unwind pairs so hot callers keep all loop registers live with zero spills.
        for &(r0, r1) in &cold_gpr_pairs {
            words.push(enc::stp_pre(r0, r1, SP, -16));
        }
        if cold_fp_bytes > 0 {
            words.push(enc::sub_imm(SP, SP, cold_fp_bytes, false));
            for (p, &(v0, v1)) in cold_fp_pairs.iter().enumerate() {
                words.push(enc::stp_q_off(v0, v1, SP, i32_of(p * 2)));
            }
        }

        if frame_size > 0 {
            let drop = 16u64 + u64::from(total_saved_bytes) + u64::from(frame_size) + (most_align - 16);
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
    let mut entry_regs = Regs::new(os);
    let mut entry_nsaa = Nsaa::new(os);
    for (p, param) in entry_params.clone().zip(func_sig.params.iter().copied()) {
        let ty = ty_at(view.tys, p);
        let Some(placed) = place_param(&param, &mut entry_regs, &mut entry_nsaa) else {
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
                           const_writes: &mut Vec<(GprCopyLoc, RematWrite), Alloc<'a>>,
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
                        AllocLoc::RematConst(cval) => const_writes.push((dst, RematWrite::Const(cval))),
                        AllocLoc::RematGlobal(g_inst) => const_writes.push((dst, RematWrite::Global(g_inst))),
                        AllocLoc::None => {
                            if let Some(cval) = at_opt(&const_u64, to_idx(src_v)) {
                                const_writes.push((dst, RematWrite::Const(cval)));
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
                        AllocLoc::RematConst(cval) => const_writes.push((dst, RematWrite::Const(cval))),
                        AllocLoc::RematGlobal(g_inst) => const_writes.push((dst, RematWrite::Global(g_inst))),
                        AllocLoc::None => {
                            if let Some(cval) = at_opt(&const_u64, to_idx(src_v)) {
                                const_writes.push((dst, RematWrite::Const(cval)));
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
                AllocLoc::None | AllocLoc::RematConst(_) | AllocLoc::RematGlobal(_) => {}
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
        // Materialize skipped constant / remat global edge arguments AFTER parallel moves so they never clobber any source!
        for &(dst, write) in const_writes.iter() {
            match dst {
                GprCopyLoc::Reg(r) => emit_remat_write_reg(words, r, write),
                GprCopyLoc::Spill(s) => {
                    emit_remat_write_reg(words, X16, write);
                    emit_st_spill_gpr(words, X16, spill_base, s);
                }
                GprCopyLoc::VecSpill(_) => {
                    internal_err.set(Some(TierError::Refused { tier: TIER, inst: None, what: "GPR const edge cannot target VecSpill" }));
                }
            }
        }
    };

    // Helper: true iff `emit_edge_moves` would emit 0 instructions for this edge (all args coalesced).
    let edge_is_nop = |target_block: u32, args_start: usize, args_len: usize| -> bool {
        let target_params = view.params(Block(target_block));
        for k in 0..args_len {
            let param_inst = target_params.start + k;
            let dst_loc = at_loc(&reg_alloc.loc_of, param_inst);
            if matches!(dst_loc, AllocLoc::None | AllocLoc::RematConst(_) | AllocLoc::RematGlobal(_)) {
                continue;
            }
            let src_v = at_u32(view.extra, args_start + k);
            let src_loc = at_loc(&reg_alloc.loc_of, to_idx(src_v));
            if dst_loc != src_loc {
                return false;
            }
        }
        true
    };

    // Precompute per-value single-block use ownership across the entire function in O(N):
    // `u32::MAX` = unused, `blk` = used ONLY in block `blk`, `u32::MAX - 1` = used in multiple blocks.
    let mut use_blk_state: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    for _ in 0..len {
        use_blk_state.push(u32::MAX);
    }
    for b in 0..blocks {
        let bu = u32_of(b);
        for inst in view.block(Block(bu)) {
            regalloc::for_each_use(&view, inst, |v| {
                let vi = to_idx(v);
                if let Some(slot) = use_blk_state.get_mut(vi) {
                    if *slot == u32::MAX {
                        *slot = bu;
                    } else if *slot != bu {
                        *slot = u32::MAX - 1;
                    }
                }
            });
        }
    }

    let mut cold_stubs: Vec<ColdEdgeStub, Alloc<'a>> = Vec::new_in(alloc);
    let mut shared_jump_tables: Vec<SharedJumpTable<'a>, Alloc<'a>> = Vec::new_in(alloc);
    let mut sme2_f32_bl_sites: Vec<usize, Alloc<'a>> = Vec::new_in(alloc);
    let mut sme2_f64_bl_sites: Vec<usize, Alloc<'a>> = Vec::new_in(alloc);
    let mut sme2_i8_i32_bl_sites: Vec<usize, Alloc<'a>> = Vec::new_in(alloc);
    let mut sme2_u8_u32_bl_sites: Vec<usize, Alloc<'a>> = Vec::new_in(alloc);
    let mut post_pairs: Vec<PostIndexPlan, Alloc<'a>> = Vec::with_capacity_in(8, alloc);
    let mut ptr_accesses: Vec<(usize, bool, PostAccessKind, i64), Alloc<'a>> = Vec::with_capacity_in(16, alloc);
    let mut skip_p_nexts: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(8, alloc);

    // Identify loop-header blocks (targets of backward edges in original or emitted block order)
    // for 16-byte fetch-boundary alignment on Apple Silicon (full 8-wide fetch bandwidth).
    let mut block_pos: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
    let mut is_loop_header: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        block_pos.push(0);
        is_loop_header.push(false);
    }
    for (pos, &blk) in block_order.iter().enumerate() {
        set_at(&mut block_pos, blk, pos);
    }
    for b in 0..blocks {
        let b_pos = at_usize(&block_pos, b);
        let term = view.terminator(Block(u32_of(b)));
        regalloc::for_each_edge(&view, term, |target, _, _| {
            let tb = to_idx(target);
            if tb <= b || at_usize(&block_pos, tb) <= b_pos {
                set_at(&mut is_loop_header, tb, true);
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
                if total_saved_bytes == 0 {
                    words.push(enc::add_imm(SP, FP, 0, false));
                } else {
                    words.push(enc::sub_imm(SP, FP, total_saved_bytes, false));
                }
            }
            if cold_fp_bytes > 0 {
                for (p, &(v0, v1)) in cold_fp_pairs.iter().enumerate() {
                    words.push(enc::ldp_q_off(v0, v1, SP, i32_of(p * 2)));
                }
                words.push(enc::add_imm(SP, SP, cold_fp_bytes, false));
            }
            for &(r0, r1) in cold_gpr_pairs.iter().rev() {
                words.push(enc::ldp_post(r0, r1, SP, 16));
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
        let is_zero_emit = |k: usize, skip: &[bool]| -> bool {
            if at_bool(skip, k) {
                return true;
            }
            if at_fusion(&fusion, k) == Fusion::SubsFlags {
                return false;
            }
            let kop = view.op(k);
            if matches!(kop, Op::Param | Op::SlotLive | Op::SlotDead | Op::Assume) {
                return true;
            }
            if !matches!(at_loc(&reg_alloc.loc_of, k), AllocLoc::None | AllocLoc::RematConst(_) | AllocLoc::RematGlobal(_)) {
                return false;
            }
            let side_eff = kop.is_terminator()
                || matches!(
                    kop,
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
            !side_eff
        };
        let load_reads_gpr = |k: usize, reg: Reg| -> bool {
            let loc_is_reg = |v: u32| match at_loc(&reg_alloc.loc_of, to_idx(v)) {
                AllocLoc::Gpr(r) => u32::from(r) == reg,
                AllocLoc::GprPair(r0, r1) => u32::from(r0) == reg || u32::from(r1) == reg,
                _ => false,
            };
            match at_fusion(&fusion, k) {
                Fusion::MemSpOff { .. } => false,
                Fusion::MemBaseOff { base, .. } => loc_is_reg(base),
                Fusion::MemScaledIdx { base, idx, .. }
                | Fusion::MemUxtwIdx { base, idx, .. }
                | Fusion::MemRegIdx { base, idx } => {
                    loc_is_reg(base) || loc_is_reg(idx)
                }
                Fusion::None => {
                    let kat = to_idx(at_u32(view.data, k));
                    loc_is_reg(at_u32(view.extra, kat))
                }
                _ => true,
            }
        };
        let try_emit_pair = |words: &mut Vec<u32, Alloc<'a>>, i1: usize, i2: usize, allow_store: bool| -> bool {
            if let (Some((base1, off1, fp1)), Some((base2, off2, fp2))) =
                (mem64_pair_addr(i1, true), mem64_pair_addr(i2, true))
            {
                let ordered = if fp1 == fp2 && base1 == base2 && off2 == off1 + 8 {
                    Some((off1, i1, i2))
                } else if fp1 == fp2 && base1 == base2 && off1 == off2 + 8 {
                    Some((off2, i2, i1))
                } else {
                    None
                };
                if let Some((lo_off, lo_i, hi_i)) = ordered
                    && lo_off % 8 == 0
                    && (-512..=504).contains(&lo_off)
                {
                    if !fp1 {
                        if let (AllocLoc::Gpr(r_first), AllocLoc::Gpr(r_lo), AllocLoc::Gpr(r_hi)) = (
                            at_loc(&reg_alloc.loc_of, i1),
                            at_loc(&reg_alloc.loc_of, lo_i),
                            at_loc(&reg_alloc.loc_of, hi_i),
                        ) {
                            let (rt_first, rt_lo, rt_hi) = (u32::from(r_first), u32::from(r_lo), u32::from(r_hi));
                            if rt_lo != rt_hi && rt_first != base1 {
                                words.push(enc::ldp_off(rt_lo, rt_hi, base1, i32_of(lo_off)));
                                return true;
                            }
                        }
                    } else if let (AllocLoc::Fp(r_lo), AllocLoc::Fp(r_hi)) =
                        (at_loc(&reg_alloc.loc_of, lo_i), at_loc(&reg_alloc.loc_of, hi_i))
                    {
                        let (dt_lo, dt_hi) = (u32::from(r_lo), u32::from(r_hi));
                        if dt_lo != dt_hi {
                            words.push(enc::ldp_d_off(dt_lo, dt_hi, base1, i32_of(lo_off)));
                            return true;
                        }
                    }
                }
            } else if allow_store
                && let (Some((base1, off1, fp1)), Some((base2, off2, fp2))) =
                    (mem64_pair_addr(i1, false), mem64_pair_addr(i2, false))
            {
                let ordered = if fp1 == fp2 && base1 == base2 && off2 == off1 + 8 {
                    Some((off1, i1, i2))
                } else if fp1 == fp2 && base1 == base2 && off1 == off2 + 8 {
                    Some((off2, i2, i1))
                } else {
                    None
                };
                if let Some((lo_off, lo_i, hi_i)) = ordered
                    && lo_off % 8 == 0
                    && (-512..=504).contains(&lo_off)
                {
                    let v_lo = at_u32(view.extra, to_idx(at_u32(view.data, lo_i)));
                    let v_hi = at_u32(view.extra, to_idx(at_u32(view.data, hi_i)));
                    if !fp1 {
                        if let (AllocLoc::Gpr(r_lo), AllocLoc::Gpr(r_hi)) =
                            (at_loc(&reg_alloc.loc_of, to_idx(v_lo)), at_loc(&reg_alloc.loc_of, to_idx(v_hi)))
                        {
                            words.push(enc::stp_off(u32::from(r_lo), u32::from(r_hi), base1, i32_of(lo_off)));
                            return true;
                        }
                    } else if let (AllocLoc::Fp(r_lo), AllocLoc::Fp(r_hi)) =
                        (at_loc(&reg_alloc.loc_of, to_idx(v_lo)), at_loc(&reg_alloc.loc_of, to_idx(v_hi)))
                    {
                        words.push(enc::stp_d_off(u32::from(r_lo), u32::from(r_hi), base1, i32_of(lo_off)));
                        return true;
                    }
                }
            } else if let (Some((base1, off1)), Some((base2, off2))) =
                (mem128_pair_addr(i1, true), mem128_pair_addr(i2, true))
            {
                // Adjacent 128-bit NEON vector pair load -> `ldp q1, q2, [base, #off]` (32 bytes in 1 op)!
                let ordered = if base1 == base2 && off2 == off1 + 16 {
                    Some((off1, i1, i2))
                } else if base1 == base2 && off1 == off2 + 16 {
                    Some((off2, i2, i1))
                } else {
                    None
                };
                if let Some((lo_off, lo_i, hi_i)) = ordered
                    && lo_off % 16 == 0
                    && (-1024..=1008).contains(&lo_off)
                {
                    if let (AllocLoc::Vec(v_lo), AllocLoc::Vec(v_hi)) =
                        (at_loc(&reg_alloc.loc_of, lo_i), at_loc(&reg_alloc.loc_of, hi_i))
                    {
                        if v_lo != v_hi {
                            words.push(enc::ldp_q_off(u32::from(v_lo), u32::from(v_hi), base1, i32_of(lo_off / 16)));
                            return true;
                        }
                    }
                }
            } else if allow_store
                && let (Some((base1, off1)), Some((base2, off2))) =
                    (mem128_pair_addr(i1, false), mem128_pair_addr(i2, false))
            {
                // Adjacent 128-bit NEON vector pair store -> `stp q1, q2, [base, #off]` (32 bytes in 1 op)!
                let ordered = if base1 == base2 && off2 == off1 + 16 {
                    Some((off1, i1, i2))
                } else if base1 == base2 && off1 == off2 + 16 {
                    Some((off2, i2, i1))
                } else {
                    None
                };
                if let Some((lo_off, lo_i, hi_i)) = ordered
                    && lo_off % 16 == 0
                    && (-1024..=1008).contains(&lo_off)
                {
                    let v_lo = at_u32(view.extra, to_idx(at_u32(view.data, lo_i)));
                    let v_hi = at_u32(view.extra, to_idx(at_u32(view.data, hi_i)));
                    if let (AllocLoc::Vec(r_lo), AllocLoc::Vec(r_hi)) =
                        (at_loc(&reg_alloc.loc_of, to_idx(v_lo)), at_loc(&reg_alloc.loc_of, to_idx(v_hi)))
                    {
                        words.push(enc::stp_q_off(u32::from(r_lo), u32::from(r_hi), base1, i32_of(lo_off / 16)));
                        return true;
                    }
                }
            }
            false
        };
        let range = view.block(Block(u32_of(b)));
        // Post-Index Pair Advance Fusion:
        // - For single-pair groups (`step == 32` Vec128 or `step == 16` Gpr64/Fp64), fold
        //   `p_next = PtrAdd(p, step)` (allocated to the SAME physical GPR `rp` as `p`) directly
        //   into a single post-index `ldp`/`stp` (`[rp], #step`) and skip `p_next`.
        // - For multi-pair groups (`step == 64` Vec128 or `step == 32` Gpr64/Fp64), emit
        //   `PostIndexPlan::Quad` in STRICTLY ASCENDING address order (`[rp, #0]` then `[rp, #half]`)
        //   and leave `PtrAdd(p, step)` (`add rp, rp, #step`) intact — zero cache-line descending
        //   reordering, zero hardware prefetcher/store-queue stalls, and zero address-dep chain!
        post_pairs.clear();
        skip_p_nexts.clear();
        let blk_u32 = u32_of(b);
        let encode_pair_word = |lo_i: usize,
                                hi_i: usize,
                                is_load: bool,
                                kind: PostAccessKind,
                                mode: PairAddrMode,
                                rp: Reg|
         -> Option<u32> {
            match (kind, is_load) {
                (PostAccessKind::Vec128, true) => {
                    let (AllocLoc::Vec(v_lo), AllocLoc::Vec(v_hi)) =
                        (at_loc(&reg_alloc.loc_of, lo_i), at_loc(&reg_alloc.loc_of, hi_i))
                    else {
                        return None;
                    };
                    if v_lo == v_hi {
                        return None;
                    }
                    let (rt1, rt2) = (u32::from(v_lo), u32::from(v_hi));
                    Some(match mode {
                        PairAddrMode::PostHalf => enc::ldp_q_post(rt1, rt2, rp, 2),
                        PairAddrMode::OffZero => enc::ldp_q_off(rt1, rt2, rp, 0),
                        PairAddrMode::OffHalf => enc::ldp_q_off(rt1, rt2, rp, 2),
                    })
                }
                (PostAccessKind::Vec128, false) => {
                    let v_lo = at_u32(view.extra, to_idx(at_u32(view.data, lo_i)));
                    let v_hi = at_u32(view.extra, to_idx(at_u32(view.data, hi_i)));
                    let (AllocLoc::Vec(r_lo), AllocLoc::Vec(r_hi)) =
                        (at_loc(&reg_alloc.loc_of, to_idx(v_lo)), at_loc(&reg_alloc.loc_of, to_idx(v_hi)))
                    else {
                        return None;
                    };
                    let (rt1, rt2) = (u32::from(r_lo), u32::from(r_hi));
                    Some(match mode {
                        PairAddrMode::PostHalf => enc::stp_q_post(rt1, rt2, rp, 2),
                        PairAddrMode::OffZero => enc::stp_q_off(rt1, rt2, rp, 0),
                        PairAddrMode::OffHalf => enc::stp_q_off(rt1, rt2, rp, 2),
                    })
                }
                (PostAccessKind::Gpr64, true) => {
                    let (AllocLoc::Gpr(r_lo), AllocLoc::Gpr(r_hi)) =
                        (at_loc(&reg_alloc.loc_of, lo_i), at_loc(&reg_alloc.loc_of, hi_i))
                    else {
                        return None;
                    };
                    let (rt_lo, rt_hi) = (u32::from(r_lo), u32::from(r_hi));
                    if rt_lo == rt_hi || rt_lo == rp || rt_hi == rp {
                        return None;
                    }
                    Some(match mode {
                        PairAddrMode::PostHalf => enc::ldp_post(rt_lo, rt_hi, rp, 16),
                        PairAddrMode::OffZero => enc::ldp_off(rt_lo, rt_hi, rp, 0),
                        PairAddrMode::OffHalf => enc::ldp_off(rt_lo, rt_hi, rp, 16),
                    })
                }
                (PostAccessKind::Gpr64, false) => {
                    let v_lo = at_u32(view.extra, to_idx(at_u32(view.data, lo_i)));
                    let v_hi = at_u32(view.extra, to_idx(at_u32(view.data, hi_i)));
                    let (AllocLoc::Gpr(r_lo), AllocLoc::Gpr(r_hi)) =
                        (at_loc(&reg_alloc.loc_of, to_idx(v_lo)), at_loc(&reg_alloc.loc_of, to_idx(v_hi)))
                    else {
                        return None;
                    };
                    let (rt_lo, rt_hi) = (u32::from(r_lo), u32::from(r_hi));
                    if rt_lo == rp || rt_hi == rp {
                        return None;
                    }
                    Some(match mode {
                        PairAddrMode::PostHalf => enc::stp_post(rt_lo, rt_hi, rp, 16),
                        PairAddrMode::OffZero => enc::stp_off(rt_lo, rt_hi, rp, 0),
                        PairAddrMode::OffHalf => enc::stp_off(rt_lo, rt_hi, rp, 16),
                    })
                }
                (PostAccessKind::Fp64, true) => {
                    let (AllocLoc::Fp(r_lo), AllocLoc::Fp(r_hi)) =
                        (at_loc(&reg_alloc.loc_of, lo_i), at_loc(&reg_alloc.loc_of, hi_i))
                    else {
                        return None;
                    };
                    let (dt_lo, dt_hi) = (u32::from(r_lo), u32::from(r_hi));
                    if dt_lo == dt_hi {
                        return None;
                    }
                    Some(match mode {
                        PairAddrMode::PostHalf => enc::ldp_post_d(dt_lo, dt_hi, rp, 16),
                        PairAddrMode::OffZero => enc::ldp_d_off(dt_lo, dt_hi, rp, 0),
                        PairAddrMode::OffHalf => enc::ldp_d_off(dt_lo, dt_hi, rp, 16),
                    })
                }
                (PostAccessKind::Fp64, false) => {
                    let v_lo = at_u32(view.extra, to_idx(at_u32(view.data, lo_i)));
                    let v_hi = at_u32(view.extra, to_idx(at_u32(view.data, hi_i)));
                    let (AllocLoc::Fp(r_lo), AllocLoc::Fp(r_hi)) =
                        (at_loc(&reg_alloc.loc_of, to_idx(v_lo)), at_loc(&reg_alloc.loc_of, to_idx(v_hi)))
                    else {
                        return None;
                    };
                    let (dt_lo, dt_hi) = (u32::from(r_lo), u32::from(r_hi));
                    Some(match mode {
                        PairAddrMode::PostHalf => enc::stp_post_d(dt_lo, dt_hi, rp, 16),
                        PairAddrMode::OffZero => enc::stp_d_off(dt_lo, dt_hi, rp, 0),
                        PairAddrMode::OffHalf => enc::stp_d_off(dt_lo, dt_hi, rp, 16),
                    })
                }
                (PostAccessKind::Other, _) => None,
            }
        };
        let build_post_pair = |a_inst: usize,
                               a_off: i64,
                               b_inst: usize,
                               b_off: i64,
                               is_load: bool,
                               kind: PostAccessKind,
                               want_lo_off: i64,
                               elem_bytes: i64,
                               rp: Reg|
         -> Option<PostIndexPlan> {
            let (lo_i, hi_i) = if a_off == want_lo_off && b_off == want_lo_off + elem_bytes {
                (a_inst, b_inst)
            } else if b_off == want_lo_off && a_off == want_lo_off + elem_bytes {
                (b_inst, a_inst)
            } else {
                return None;
            };
            // Verify intervening instructions strictly between `a_inst` and `b_inst` are safely pairable.
            let mut mid_opt: Option<usize> = None;
            for k in (a_inst + 1)..b_inst {
                if is_zero_emit(k, &skip_emit) {
                    continue;
                }
                if !is_load || mid_opt.is_some() {
                    return None;
                }
                mid_opt = Some(k);
            }
            if let Some(mid) = mid_opt {
                if view.op(mid) != Op::Load || Flags(at_u8(view.flags, mid)).contains(Flags::VOLATILE) {
                    return None;
                }
                let loc1 = at_loc(&reg_alloc.loc_of, mid);
                let loc2 = at_loc(&reg_alloc.loc_of, b_inst);
                if !matches!(loc1, AllocLoc::Gpr(_) | AllocLoc::Fp(_) | AllocLoc::Vec(_)) {
                    return None;
                }
                let clobbers_base = matches!(loc1, AllocLoc::Gpr(r) if u32::from(r) == rp);
                let dst_hazard = loc1 == loc2
                    || matches!(loc2, AllocLoc::Gpr(r2) if load_reads_gpr(mid, u32::from(r2)));
                if clobbers_base || dst_hazard || load_reads_gpr(mid, rp) {
                    return None;
                }
            }
            let word = encode_pair_word(lo_i, hi_i, is_load, kind, PairAddrMode::PostHalf, rp)?;
            Some(PostIndexPlan::Single { first_inst: a_inst, second_inst: b_inst, word })
        };
        let build_post_quad = |a0: (usize, bool, PostAccessKind, i64),
                               a1: (usize, bool, PostAccessKind, i64),
                               a2: (usize, bool, PostAccessKind, i64),
                               a3: (usize, bool, PostAccessKind, i64),
                               elem_bytes: i64,
                               rp: Reg,
                               pending_skips: &[usize]|
         -> Option<PostIndexPlan> {
            let is_load = a0.1;
            let kind = a0.2;
            let mut lo0_opt = None;
            let mut lo1_opt = None;
            let mut hi0_opt = None;
            let mut hi1_opt = None;
            for &(idx, _, _, off) in &[a0, a1, a2, a3] {
                if off == 0 {
                    lo0_opt = Some(idx);
                } else if off == elem_bytes {
                    lo1_opt = Some(idx);
                } else if off == 2 * elem_bytes {
                    hi0_opt = Some(idx);
                } else if off == 3 * elem_bytes {
                    hi1_opt = Some(idx);
                } else {
                    return None;
                }
            }
            let (lo0_i, lo1_i, hi0_i, hi1_i) = (lo0_opt?, lo1_opt?, hi0_opt?, hi1_opt?);
            let loc0 = at_loc(&reg_alloc.loc_of, lo0_i);
            let loc1 = at_loc(&reg_alloc.loc_of, lo1_i);
            let loc2 = at_loc(&reg_alloc.loc_of, hi0_i);
            let loc3 = at_loc(&reg_alloc.loc_of, hi1_i);
            if is_load
                && (loc0 == loc1
                    || loc0 == loc2
                    || loc0 == loc3
                    || loc1 == loc2
                    || loc1 == loc3
                    || loc2 == loc3)
            {
                return None;
            }
            // Verify every instruction strictly between `a0.0` and `a3.0` (excluding `a1.0`, `a2.0`)
            // is either zero-emit (for loads & stores) or an independent register Load (loads only).
            for k in (a0.0 + 1)..a3.0 {
                if k == a1.0 || k == a2.0 || is_zero_emit(k, &skip_emit) || pending_skips.iter().any(|&pn| pn == k) {
                    continue;
                }
                if !is_load {
                    return None;
                }
                if view.op(k) != Op::Load || Flags(at_u8(view.flags, k)).contains(Flags::VOLATILE) {
                    return None;
                }
                let loc_k = at_loc(&reg_alloc.loc_of, k);
                if !matches!(loc_k, AllocLoc::Gpr(_) | AllocLoc::Fp(_) | AllocLoc::Vec(_)) {
                    return None;
                }
                let clobbers_base = matches!(loc_k, AllocLoc::Gpr(r) if u32::from(r) == rp);
                let dst_collision = loc_k == loc0 || loc_k == loc1 || loc_k == loc2 || loc_k == loc3;
                let gpr_read_hazard = [loc0, loc1, loc2, loc3]
                    .iter()
                    .any(|loc| matches!(*loc, AllocLoc::Gpr(r) if load_reads_gpr(k, u32::from(r))));
                if clobbers_base || dst_collision || gpr_read_hazard || load_reads_gpr(k, rp) {
                    return None;
                }
            }
            let lo_word = encode_pair_word(lo0_i, lo1_i, is_load, kind, PairAddrMode::OffZero, rp)?;
            let hi_word = encode_pair_word(hi0_i, hi1_i, is_load, kind, PairAddrMode::OffHalf, rp)?;
            Some(PostIndexPlan::Quad { i0: a0.0, i1: a1.0, i2: a2.0, i3: a3.0, lo_word, hi_word })
        };
        let range_preserves_rp = |start_i: usize, cand_i: usize, rp: Reg| -> bool {
            for k in start_i..=cand_i {
                if k != cand_i && matches!(at_loc(&reg_alloc.loc_of, k), AllocLoc::Gpr(r) if u32::from(r) == rp) {
                    return false;
                }
                if !at_bool(&skip_emit, k)
                    && matches!(
                        view.op(k),
                        Op::Call
                            | Op::CallIndirect
                            | Op::Invoke
                            | Op::InvokeIndirect
                            | Op::Memcpy
                            | Op::Memmove
                            | Op::Memset
                            | Op::Memcmp
                            | Op::TlsAddr
                    )
                {
                    return false;
                }
            }
            true
        };
        for cand in range.start..range.end {
            if at_bool(&skip_emit, cand) || view.op(cand) != Op::PtrAdd || at_fusion(&fusion, cand) != Fusion::None {
                continue;
            }
            let cat = to_idx(at_u32(view.data, cand));
            let p_u32 = at_u32(view.extra, cat);
            let by_u32 = at_u32(view.extra, cat + 1);
            let p_idx = to_idx(p_u32);
            if p_idx == cand || by_u32 == p_u32 {
                continue;
            }
            let Some(cv) = at_opt(&const_u64, to_idx(by_u32)) else {
                continue;
            };
            let step = sign_extend_u64(cv, ty_at(view.tys, to_idx(by_u32)).bits(64));
            if !matches!(step, 16 | 32 | 64) {
                continue;
            }
            let (AllocLoc::Gpr(rp_p), AllocLoc::Gpr(rp_n)) =
                (at_loc(&reg_alloc.loc_of, p_idx), at_loc(&reg_alloc.loc_of, cand))
            else {
                continue;
            };
            if rp_p != rp_n {
                continue;
            }
            let rp = u32::from(rp_p);
            if at_u32(&use_blk_state, p_idx) != blk_u32 {
                continue;
            }
            // Verify EVERY raw LIR use of `p` in block `b` is either `cand`, a direct non-volatile
            // Load/Store `< cand`, or a skipped `PtrAdd`/`Add` used solely by `MemBaseOff { base: p }` `< cand`.
            let mut all_uses_safe = true;
            for k in range.start..range.end {
                let mut p_uses_in_k = 0u32;
                regalloc::for_each_use(&view, k, |v| {
                    if v == p_u32 {
                        p_uses_in_k += 1;
                    }
                });
                if p_uses_in_k == 0 {
                    continue;
                }
                if k == cand {
                    if p_uses_in_k != 1 {
                        all_uses_safe = false;
                        break;
                    }
                    continue;
                }
                if at_bool(&skip_emit, k) && matches!(view.op(k), Op::PtrAdd | Op::Add) {
                    let k_u32 = u32_of(k);
                    if at_u32(&use_blk_state, k) != blk_u32 {
                        all_uses_safe = false;
                        break;
                    }
                    for m in (k + 1)..range.end {
                        let mut k_uses_in_m = 0u32;
                        regalloc::for_each_use(&view, m, |v| {
                            if v == k_u32 {
                                k_uses_in_m += 1;
                            }
                        });
                        if k_uses_in_m == 0 {
                            continue;
                        }
                        let mop = view.op(m);
                        let ok_mem = m < cand
                            && !at_bool(&skip_emit, m)
                            && matches!(mop, Op::Load | Op::Store)
                            && !Flags(at_u8(view.flags, m)).contains(Flags::VOLATILE)
                            && matches!(at_fusion(&fusion, m), Fusion::MemBaseOff { base, .. } if base == p_u32)
                            && (mop == Op::Load || at_u32(view.extra, to_idx(at_u32(view.data, m))) != k_u32);
                        if !ok_mem {
                            all_uses_safe = false;
                            break;
                        }
                    }
                    if !all_uses_safe {
                        break;
                    }
                    continue;
                }
                let kop = view.op(k);
                let ok_direct = k < cand
                    && !at_bool(&skip_emit, k)
                    && matches!(kop, Op::Load | Op::Store)
                    && !Flags(at_u8(view.flags, k)).contains(Flags::VOLATILE)
                    && at_fusion(&fusion, k) == Fusion::None
                    && if kop == Op::Load {
                        at_u32(view.extra, to_idx(at_u32(view.data, k))) == p_u32
                    } else {
                        let kat = to_idx(at_u32(view.data, k));
                        at_u32(view.extra, kat) != p_u32 && at_u32(view.extra, kat + 1) == p_u32
                    };
                if !ok_direct {
                    all_uses_safe = false;
                    break;
                }
            }
            if !all_uses_safe {
                continue;
            }
            ptr_accesses.clear();
            for k in range.start..cand {
                if at_bool(&skip_emit, k) {
                    continue;
                }
                let kop = view.op(k);
                if !matches!(kop, Op::Load | Op::Store) {
                    continue;
                }
                let is_load = kop == Op::Load;
                let kat = to_idx(at_u32(view.data, k));
                let acc_off = match at_fusion(&fusion, k) {
                    Fusion::MemBaseOff { base, off, .. } if base == p_u32 => Some(off),
                    Fusion::None => {
                        let (ptr_v, off) = if is_load {
                            (at_u32(view.extra, kat), i64::from(at_u32(view.extra, kat + 1).cast_signed()))
                        } else {
                            (at_u32(view.extra, kat + 1), i64::from(at_u32(view.extra, kat + 2).cast_signed()))
                        };
                        (ptr_v == p_u32).then_some(off)
                    }
                    _ => None,
                };
                let Some(off) = acc_off else {
                    continue;
                };
                let acc_ty = if is_load {
                    ty_at(view.tys, k)
                } else {
                    ty_at(view.tys, to_idx(at_u32(view.extra, kat)))
                };
                let kind = if is_vec128_ty(acc_ty).is_some() {
                    PostAccessKind::Vec128
                } else if let Some((Size::X, is_fp)) = mem_size_of(acc_ty) {
                    if is_fp { PostAccessKind::Fp64 } else { PostAccessKind::Gpr64 }
                } else {
                    PostAccessKind::Other
                };
                ptr_accesses.push((k, is_load, kind, off));
            }
            let n_acc = ptr_accesses.len();
            let already_claimed = |pairs: &[PostIndexPlan], idx: usize| {
                pairs.iter().any(|pp| pp.contains(idx))
            };
            // Case 1 (2x 128-bit pairs, step=64) & Case 2 (2x 64-bit pairs, step=32):
            let mut matched = false;
            if n_acc >= 4
                && matches!(step, 32 | 64)
                && let (Some(&a0), Some(&a1), Some(&a2), Some(&a3)) = (
                    ptr_accesses.get(n_acc - 4),
                    ptr_accesses.get(n_acc - 3),
                    ptr_accesses.get(n_acc - 2),
                    ptr_accesses.get(n_acc - 1),
                )
            {
                let want_kind_ok = (step == 64 && a0.2 == PostAccessKind::Vec128)
                    || (step == 32 && matches!(a0.2, PostAccessKind::Gpr64 | PostAccessKind::Fp64));
                if want_kind_ok
                    && a1.1 == a0.1
                    && a2.1 == a0.1
                    && a3.1 == a0.1
                    && a1.2 == a0.2
                    && a2.2 == a0.2
                    && a3.2 == a0.2
                    && !already_claimed(&post_pairs, a0.0)
                    && !already_claimed(&post_pairs, a1.0)
                    && !already_claimed(&post_pairs, a2.0)
                    && !already_claimed(&post_pairs, a3.0)
                    && range_preserves_rp(a0.0, cand, rp)
                {
                    let elem_bytes = if a0.2 == PostAccessKind::Vec128 { 16 } else { 8 };
                    if let Some(quad) = build_post_quad(a0, a1, a2, a3, elem_bytes, rp, &skip_p_nexts) {
                        post_pairs.push(quad);
                        matched = true;
                    }
                }
            }
            // Case 3 (1x 128-bit pair, step=32; or 1x 64-bit pair, step=16):
            if !matched
                && n_acc >= 2
                && matches!(step, 16 | 32)
                && let (Some(&a0), Some(&a1)) = (ptr_accesses.get(n_acc - 2), ptr_accesses.get(n_acc - 1))
            {
                let want_kind_ok = (step == 32 && a0.2 == PostAccessKind::Vec128)
                    || (step == 16 && matches!(a0.2, PostAccessKind::Gpr64 | PostAccessKind::Fp64));
                if want_kind_ok
                    && a1.1 == a0.1
                    && a1.2 == a0.2
                    && !already_claimed(&post_pairs, a0.0)
                    && !already_claimed(&post_pairs, a1.0)
                    && range_preserves_rp(a0.0, cand, rp)
                {
                    let elem_bytes = if a0.2 == PostAccessKind::Vec128 { 16 } else { 8 };
                    if let Some(p1) = build_post_pair(a0.0, a0.3, a1.0, a1.3, a0.1, a0.2, 0, elem_bytes, rp) {
                        post_pairs.push(p1);
                        skip_p_nexts.push(cand);
                    }
                }
            }
        }
        for &pn in &skip_p_nexts {
            set_at(&mut skip_emit, pn, true);
        }

        let mut inst = range.start;
        while inst < range.end {
            if at_bool(&skip_emit, inst) {
                inst += 1;
                continue;
            }
            if let Some(pp) = post_pairs.iter().find(|pp| pp.trigger_inst() == inst).copied() {
                match pp {
                    PostIndexPlan::Single { second_inst, word, .. } => {
                        words.push(word);
                        set_at(&mut skip_emit, second_inst, true);
                    }
                    PostIndexPlan::Quad { i1, i2, i3, lo_word, hi_word, .. } => {
                        words.push(lo_word);
                        words.push(hi_word);
                        set_at(&mut skip_emit, i1, true);
                        set_at(&mut skip_emit, i2, true);
                        set_at(&mut skip_emit, i3, true);
                    }
                }
                inst += 1;
                continue;
            }
            // Generalized 64-bit (`ldp x/d` / `stp x/d`) & 128-bit NEON (`ldp q` / `stp q`) pair fusion:
            // skips folded/zero-emit instructions (`PtrAdd`/`Iconst`) between consecutive accesses, AND
            // pairs 1-interleaved loads (`a0, b0, a1, b1` -> `ldp a0, a1` + `ldp b0, b1`) safely!
            let is_post_member = |idx: usize| post_pairs.iter().any(|pp| pp.contains(idx));
            let mut next1 = inst + 1;
            while next1 < range.end && is_zero_emit(next1, &skip_emit) {
                next1 += 1;
            }
            if next1 < range.end {
                if !is_post_member(next1) && try_emit_pair(&mut words, inst, next1, true) {
                    set_at(&mut skip_emit, next1, true);
                    inst += 1;
                    continue;
                }
                // Check 1-interleaved load pairing across an independent register-resident non-volatile Load.
                let next1_is_reg_load = view.op(next1) == Op::Load
                    && !Flags(at_u8(view.flags, next1)).contains(Flags::VOLATILE)
                    && matches!(at_loc(&reg_alloc.loc_of, next1), AllocLoc::Gpr(_) | AllocLoc::Fp(_) | AllocLoc::Vec(_));
                if view.op(inst) == Op::Load && next1_is_reg_load {
                    let mut next2 = next1 + 1;
                    while next2 < range.end && is_zero_emit(next2, &skip_emit) {
                        next2 += 1;
                    }
                    if next2 < range.end && view.op(next2) == Op::Load && !is_post_member(next2) {
                        let base_of_inst = mem128_pair_addr(inst, true)
                            .map(|(b_reg, _)| b_reg)
                            .or_else(|| mem64_pair_addr(inst, true).map(|(b_reg, _, _)| b_reg));
                        if let Some(base1) = base_of_inst {
                            let loc1 = at_loc(&reg_alloc.loc_of, next1);
                            let loc2 = at_loc(&reg_alloc.loc_of, next2);
                            let clobbers_base = matches!(loc1, AllocLoc::Gpr(r) if u32::from(r) == base1);
                            let dst_hazard = loc1 == loc2
                                || matches!(loc2, AllocLoc::Gpr(r2) if load_reads_gpr(next1, u32::from(r2)));
                            if !clobbers_base && !dst_hazard && try_emit_pair(&mut words, inst, next2, false) {
                                set_at(&mut skip_emit, next2, true);
                                inst += 1;
                                continue;
                            }
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
            if !has_side_effect
                && matches!(loc, AllocLoc::None | AllocLoc::RematConst(_) | AllocLoc::RematGlobal(_))
                && at_fusion(&fusion, inst) != Fusion::SubsFlags
            {
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
                        } else if vsize == 1 {
                            words.push(if op == Op::Shl { shl_8h_imm(vd, vs, k) } else { ushr_8h_imm(vd, vs, k) });
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
                            match at_fusion(&fusion, inst) {
                                Fusion::VecUadalp { acc, src_4s } => {
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
                                Fusion::Madd { a, b: mb, acc } => {
                                    let va = load_vec(&mut words, a, V29);
                                    let vb = load_vec(&mut words, mb, V30);
                                    let vc = load_vec(&mut words, acc, V31);
                                    emit_vec_acc3(&mut words, vd, va, vb, vc, |d, n, m| enc::mla_v(vsize, d, n, m));
                                    if let Some(s) = sp {
                                        emit_st_spill_vec(&mut words, vd, spill_base, s);
                                    }
                                    inst += 1;
                                    continue;
                                }
                                _ => {}
                            }
                        }
                        if op == Op::Sub {
                            if let Fusion::Msub { a, b: mb, acc } = at_fusion(&fusion, inst) {
                                let va = load_vec(&mut words, a, V29);
                                let vb = load_vec(&mut words, mb, V30);
                                let vc = load_vec(&mut words, acc, V31);
                                emit_vec_acc3(&mut words, vd, va, vb, vc, |d, n, m| enc::mls_v(vsize, d, n, m));
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
                        } else if vsize == 3 && matches!(op, Op::Umin | Op::Umax | Op::Smin | Op::Smax) {
                            // `<2 x i64>` vector min/max via `cmgt.2d`/`cmhi.2d` + `bsl`/`bit`/`bif`!
                            let unsigned = matches!(op, Op::Umin | Op::Umax);
                            let is_min = matches!(op, Op::Umin | Op::Smin);
                            let (lhs_cmp, rhs_cmp) = if is_min { (vb, va) } else { (va, vb) };
                            words.push(enc::cmgt_v(unsigned, 3, V29, lhs_cmp, rhs_cmp));
                            if vd == vb {
                                words.push(enc::bit_v(vd, va, V29));
                            } else if vd == va {
                                words.push(enc::bif_v(vd, vb, V29));
                            } else {
                                words.push(enc::bsl_v(V29, va, vb));
                                words.push(enc::orr_v(vd, V29, V29));
                            }
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
                    Op::Fadd | Op::Fsub | Op::Fmul | Op::Fma | Op::Fmuladd => {
                        let (vd, sp) = dst_vec(inst);
                        let double = vsize == 3;
                        if matches!(op, Op::Fma | Op::Fmuladd) {
                            if let Fusion::VecFmaddElem { vec, scalar, acc } = at_fusion(&fusion, inst) {
                                let vv = load_vec(&mut words, vec, V29);
                                let vs = load_fp(&mut words, scalar, V30);
                                let vc = load_vec(&mut words, acc, V31);
                                emit_vec_acc3(&mut words, vd, vv, vs, vc, |d, n, m| fmla_elem0_v(double, d, n, m));
                            } else {
                                let at = to_idx(at_u32(view.data, inst));
                                let va = load_vec(&mut words, at_u32(view.extra, at), V29);
                                let vb = load_vec(&mut words, at_u32(view.extra, at + 1), V30);
                                let vc = load_vec(&mut words, at_u32(view.extra, at + 2), V31);
                                emit_vec_acc3(&mut words, vd, va, vb, vc, |d, n, m| enc::fmla_v(double, d, n, m));
                            }
                            if let Some(s) = sp {
                                emit_st_spill_vec(&mut words, vd, spill_base, s);
                            }
                            inst += 1;
                            continue;
                        }
                        if op == Op::Fadd {
                            match at_fusion(&fusion, inst) {
                                Fusion::VecFmaddElem { vec, scalar, acc } => {
                                    let vv = load_vec(&mut words, vec, V29);
                                    let vs = load_fp(&mut words, scalar, V30);
                                    let vc = load_vec(&mut words, acc, V31);
                                    emit_vec_acc3(&mut words, vd, vv, vs, vc, |d, n, m| fmla_elem0_v(double, d, n, m));
                                    if let Some(s) = sp {
                                        emit_st_spill_vec(&mut words, vd, spill_base, s);
                                    }
                                    inst += 1;
                                    continue;
                                }
                                Fusion::Fmadd { a, b: mb, acc } => {
                                    let va = load_vec(&mut words, a, V29);
                                    let vb = load_vec(&mut words, mb, V30);
                                    let vc = load_vec(&mut words, acc, V31);
                                    emit_vec_acc3(&mut words, vd, va, vb, vc, |d, n, m| enc::fmla_v(double, d, n, m));
                                    if let Some(s) = sp {
                                        emit_st_spill_vec(&mut words, vd, spill_base, s);
                                    }
                                    inst += 1;
                                    continue;
                                }
                                _ => {}
                            }
                        }
                        if op == Op::Fsub {
                            match at_fusion(&fusion, inst) {
                                Fusion::VecFmsubElem { vec, scalar, acc } => {
                                    let vv = load_vec(&mut words, vec, V29);
                                    let vs = load_fp(&mut words, scalar, V30);
                                    let vc = load_vec(&mut words, acc, V31);
                                    emit_vec_acc3(&mut words, vd, vv, vs, vc, |d, n, m| fmls_elem0_v(double, d, n, m));
                                    if let Some(s) = sp {
                                        emit_st_spill_vec(&mut words, vd, spill_base, s);
                                    }
                                    inst += 1;
                                    continue;
                                }
                                Fusion::Fmsub { a, b: mb, acc } => {
                                    let va = load_vec(&mut words, a, V29);
                                    let vb = load_vec(&mut words, mb, V30);
                                    let vc = load_vec(&mut words, acc, V31);
                                    emit_vec_acc3(&mut words, vd, va, vb, vc, |d, n, m| enc::fmls_v(double, d, n, m));
                                    if let Some(s) = sp {
                                        emit_st_spill_vec(&mut words, vd, spill_base, s);
                                    }
                                    inst += 1;
                                    continue;
                                }
                                _ => {}
                            }
                        }
                        if op == Op::Fmul {
                            if let Fusion::VecFmulElem { vec, scalar } = at_fusion(&fusion, inst) {
                                let vv = load_vec(&mut words, vec, V30);
                                let vs = load_fp(&mut words, scalar, V31);
                                words.push(fmul_elem0_v(double, vd, vv, vs));
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
                    Op::Popcnt => {
                        let (vd, sp) = dst_vec(inst);
                        let vs = load_vec(&mut words, at_u32(view.data, inst), V30);
                        words.push(cnt_16b(vd, vs));
                        if vsize >= 1 {
                            words.push(uaddlp_v(0, vd, vd));
                        }
                        if vsize >= 2 {
                            words.push(uaddlp_v(1, vd, vd));
                        }
                        if vsize >= 3 {
                            words.push(uaddlp_v(2, vd, vd));
                        }
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
                    Op::TargetIntrinsic => {
                        let at = to_idx(at_u32(view.data, inst));
                        let name = view.intrinsic_name(IntrinsicId(at_u32(view.extra, at)));
                        let (vd, sp) = dst_vec(inst);
                        if name == b"blaze.neon.tbl16" {
                            let vt = load_vec(&mut words, at_u32(view.extra, at + 2), V29);
                            let vi = load_vec(&mut words, at_u32(view.extra, at + 3), V30);
                            words.push(super::sme2::tbl1_16b(vd, vt, vi));
                        } else if name == b"blaze.neon.tbl64" {
                            let v0 = at_u32(view.extra, at + 2);
                            let v1 = at_u32(view.extra, at + 3);
                            let v2 = at_u32(view.extra, at + 4);
                            let v3 = at_u32(view.extra, at + 5);
                            let vi_val = at_u32(view.extra, at + 6);
                            let consec = match (
                                at_loc(&reg_alloc.loc_of, to_idx(v0)),
                                at_loc(&reg_alloc.loc_of, to_idx(v1)),
                                at_loc(&reg_alloc.loc_of, to_idx(v2)),
                                at_loc(&reg_alloc.loc_of, to_idx(v3)),
                            ) {
                                (AllocLoc::Fp(r0), AllocLoc::Fp(r1), AllocLoc::Fp(r2), AllocLoc::Fp(r3))
                                    if ((r0 + 1) & 31) == r1 && ((r0 + 2) & 31) == r2 && ((r0 + 3) & 31) == r3 =>
                                {
                                    Some(u32::from(r0))
                                }
                                _ => None,
                            };
                            if let Some(base_r) = consec {
                                let vi = load_vec(&mut words, vi_val, V31);
                                words.push(super::sme2::tbl4_16b(vd, base_r, vi));
                            } else {
                                // Stackless 2x `tbl2_16b` split using reserved scratch `V29, V30, V31`:
                                // indices 0..31 lookup in `{t0,t1}`, indices 32..63 shifted by -32 lookup in `{t2,t3}`.
                                let vi = load_vec(&mut words, vi_val, V31);
                                let ri_safe = if vi == vd {
                                    words.push(enc::orr_v(V31, vi, vi));
                                    V31
                                } else {
                                    vi
                                };
                                let rt0 = load_vec(&mut words, v0, V29);
                                if rt0 != V29 {
                                    words.push(enc::orr_v(V29, rt0, rt0));
                                }
                                let rt1 = load_vec(&mut words, v1, V30);
                                if rt1 != V30 {
                                    words.push(enc::orr_v(V30, rt1, rt1));
                                }
                                words.push(super::sme2::tbl2_16b(vd, V29, ri_safe));
                                // `movi V29.16b, #32` (0x4f01_e400 | V29) -> `sub V31.16b, ri_safe, V29`
                                words.push(0x4f01_e400 | V29);
                                words.push(enc::sub_v(0, V31, ri_safe, V29));
                                let rt2 = load_vec(&mut words, v2, V29);
                                if rt2 != V29 {
                                    words.push(enc::orr_v(V29, rt2, rt2));
                                }
                                let rt3 = load_vec(&mut words, v3, V30);
                                if rt3 != V30 {
                                    words.push(enc::orr_v(V30, rt3, rt3));
                                }
                                words.push(super::sme2::tbl2_16b(V31, V29, V31));
                                words.push(enc::orr_v(vd, vd, V31));
                            }
                        } else if name == b"blaze.neon.sdot" {
                            let vc = load_vec(&mut words, at_u32(view.extra, at + 2), V31);
                            let va = load_vec(&mut words, at_u32(view.extra, at + 3), V29);
                            let vb = load_vec(&mut words, at_u32(view.extra, at + 4), V30);
                            emit_vec_acc3(&mut words, vd, va, vb, vc, super::sme2::sdot_4s);
                        } else if name == b"blaze.neon.udot" {
                            let vc = load_vec(&mut words, at_u32(view.extra, at + 2), V31);
                            let va = load_vec(&mut words, at_u32(view.extra, at + 3), V29);
                            let vb = load_vec(&mut words, at_u32(view.extra, at + 4), V30);
                            emit_vec_acc3(&mut words, vd, va, vb, vc, super::sme2::udot_4s);
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
            if ty == Ty::I128 && !matches!(op, Op::Call | Op::CallIndirect | Op::Invoke | Op::InvokeIndirect) {
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
                        let (lhs_src, rhs_src) = match at_fusion(&fusion, inst) {
                            Fusion::I128WideMul { lhs_src, rhs_src } => (lhs_src, rhs_src),
                            _ => (None, None),
                        };
                        let a_hi_zero = lhs_src.is_some() || i128_hi_is_zero(&view, a);
                        let b_hi_zero = rhs_src.is_some() || i128_hi_is_zero(&view, bv);
                        if let Some((s0, s1)) = sp_pair {
                            let a0 = match lhs_src {
                                Some(src) => load_gpr(&mut words, src, X16),
                                None => load_half(&mut words, a, false, X16),
                            };
                            let b0 = match rhs_src {
                                Some(src) => load_gpr(&mut words, src, X17),
                                None => load_half(&mut words, bv, false, X17),
                            };
                            words.push(enc::mul(X16, a0, b0));
                            emit_st_spill_gpr(&mut words, X16, spill_base, s0);
                            let a0 = match lhs_src {
                                Some(src) => load_gpr(&mut words, src, X16),
                                None => load_half(&mut words, a, false, X16),
                            };
                            let b0 = match rhs_src {
                                Some(src) => load_gpr(&mut words, src, X17),
                                None => load_half(&mut words, bv, false, X17),
                            };
                            words.push(enc::umulh(X16, a0, b0));
                            emit_st_spill_gpr(&mut words, X16, spill_base, s1);
                            if !b_hi_zero {
                                let a0 = match lhs_src {
                                    Some(src) => load_gpr(&mut words, src, X16),
                                    None => load_half(&mut words, a, false, X16),
                                };
                                let b1 = load_half(&mut words, bv, true, X17);
                                words.push(enc::mul(X16, a0, b1));
                                emit_ld_spill_gpr(&mut words, X17, spill_base, s1);
                                words.push(enc::add(X16, X17, X16));
                                emit_st_spill_gpr(&mut words, X16, spill_base, s1);
                            }
                            if !a_hi_zero {
                                let a1 = load_half(&mut words, a, true, X16);
                                let b0 = match rhs_src {
                                    Some(src) => load_gpr(&mut words, src, X17),
                                    None => load_half(&mut words, bv, false, X17),
                                };
                                words.push(enc::mul(X16, a1, b0));
                                emit_ld_spill_gpr(&mut words, X17, spill_base, s1);
                                words.push(enc::add(X16, X17, X16));
                                emit_st_spill_gpr(&mut words, X16, spill_base, s1);
                            }
                        } else {
                            let a0 = match lhs_src {
                                Some(src) => load_gpr(&mut words, src, X16),
                                None => load_half(&mut words, a, false, X16),
                            };
                            let b0 = match rhs_src {
                                Some(src) => load_gpr(&mut words, src, X17),
                                None => load_half(&mut words, bv, false, X17),
                            };
                            words.push(enc::umulh(dhi, a0, b0));
                            words.push(enc::mul(dlo, a0, b0));
                            if !b_hi_zero {
                                let b1 = load_half(&mut words, bv, true, X17);
                                words.push(enc::madd(dhi, a0, b1, dhi));
                            }
                            if !a_hi_zero {
                                let a1 = load_half(&mut words, a, true, X16);
                                let b0 = match rhs_src {
                                    Some(src) => load_gpr(&mut words, src, X17),
                                    None => load_half(&mut words, bv, false, X17),
                                };
                                words.push(enc::madd(dhi, a1, b0, dhi));
                            }
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
                                if has_cssc {
                                    words.push(enc::cssc_minmax(unsigned, is_min, rd, X16, X17));
                                } else {
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
            if op == Op::Extract {
                let at = to_idx(at_u32(view.data, inst));
                let vec_v = at_u32(view.extra, at);
                let lane = at_u32(view.extra, at + 1);
                let (vsize, vfp) = is_vec128_ty(ty_at(view.tys, to_idx(vec_v))).ok_or(TierError::Refused {
                    tier: TIER,
                    inst: Some(u32_of(inst)),
                    what: "non-vec128 extract",
                })?;
                if vfp {
                    let double = vsize == 3;
                    let (vd, sp) = dst_fp(inst);
                    let vs = load_vec(&mut words, vec_v, V30);
                    if lane == 0 {
                        if vd != vs {
                            words.push(enc::fmov_reg(double, vd, vs));
                        }
                    } else {
                        words.push(dup_scalar_elem(vsize, lane, vd, vs));
                    }
                    if let Some(s) = sp {
                        emit_st_spill_fp(&mut words, vd, spill_base, s);
                    }
                } else {
                    let (rd, sp) = dst_gpr(inst);
                    let vs = load_vec(&mut words, vec_v, V30);
                    words.push(enc::umov_lane(vsize, lane, rd, vs));
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
                            let raw = if size == Size::B {
                                enc::ldr_reg(size, dst, rbase, ridx)
                            } else {
                                enc::ldr_reg_scaled(size, dst, rbase, ridx)
                            };
                            words.push(if is_fp { enc::fp_access(raw) } else { raw });
                        }
                        Fusion::MemUxtwIdx { base, idx, scaled, .. } => {
                            let rbase = load_gpr(&mut words, base, X16);
                            let ridx = load_gpr(&mut words, idx, X17);
                            let raw = if scaled {
                                enc::ldr_reg_uxtw_scaled(size, dst, rbase, ridx)
                            } else {
                                enc::ldr_reg_uxtw(size, dst, rbase, ridx)
                            };
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
                                    words.push(if size == Size::B {
                                        enc::str_reg(size, rval, rbase, ridx)
                                    } else {
                                        enc::str_reg_scaled(size, rval, rbase, ridx)
                                    });
                                } else {
                                    let rbase = load_gpr(&mut words, base, X16);
                                    let ridx = load_gpr(&mut words, idx, X17);
                                    words.push(enc::add_lsl(X17, rbase, ridx, mem_shift(size)));
                                    let rval = load_gpr(&mut words, val, X16);
                                    emit_mem_imm(&mut words, false, false, size, rval, X17, 0);
                                }
                            }
                        }
                        Fusion::MemUxtwIdx { base, idx, scaled, .. } => {
                            if is_fp {
                                let rval = load_fp(&mut words, val, V30);
                                let rbase = load_gpr(&mut words, base, X16);
                                let ridx = load_gpr(&mut words, idx, X17);
                                let raw = if scaled {
                                    enc::str_reg_uxtw_scaled(size, rval, rbase, ridx)
                                } else {
                                    enc::str_reg_uxtw(size, rval, rbase, ridx)
                                };
                                words.push(enc::fp_access(raw));
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
                                    let raw = if scaled {
                                        enc::str_reg_uxtw_scaled(size, rval, rbase, ridx)
                                    } else {
                                        enc::str_reg_uxtw(size, rval, rbase, ridx)
                                    };
                                    words.push(raw);
                                } else {
                                    let rbase = load_gpr(&mut words, base, X16);
                                    let ridx = load_gpr(&mut words, idx, X17);
                                    emit_narrow(&mut words, ridx, 32);
                                    if scaled {
                                        words.push(enc::add_lsl(X17, rbase, ridx, mem_shift(size)));
                                    } else {
                                        words.push(enc::add(X17, rbase, ridx));
                                    }
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
                                AllocLoc::RematConst(cval) => const_edge_writes.push((dst, RematWrite::Const(cval))),
                                AllocLoc::RematGlobal(g_inst) => const_edge_writes.push((dst, RematWrite::Global(g_inst))),
                                AllocLoc::None => {
                                    let cval = at_opt(&const_u64, to_idx(arg_v)).ok_or(TierError::Refused {
                                        tier: TIER,
                                        inst: Some(u32_of(inst)),
                                        what: "unallocated memcpy arg",
                                    })?;
                                    const_edge_writes.push((dst, RematWrite::Const(cval)));
                                }
                                _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unexpected memcpy arg loc" }),
                            }
                        }
                        emit_parallel_gpr_moves(&mut words, &mut gpr_moves, spill_base)?;
                        for &(dst, write) in const_edge_writes.iter() {
                            if let GprCopyLoc::Reg(r) = dst {
                                emit_remat_write_reg(&mut words, r, write);
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
                    // Overlap-safe inline NEON `memmove` for constant `len <= 32 B`: load ALL source bytes
                    // into permanently reserved scratch `V30`/`V31` FIRST, then store ALL bytes to `dst`
                    // SECOND (zero GPR pressure, zero call clobbers, exact for forward & backward overlap).
                    let at = to_idx(at_u32(view.data, inst));
                    let dst_v = at_u32(view.extra, at);
                    let src_v = at_u32(view.extra, at + 1);
                    let len_v = at_u32(view.extra, at + 2);
                    if let Some(n) = small_const_memmove_len(&view, inst, &const_u64) {
                        let orig_len = to_idx(n);
                        if orig_len > 0 {
                            let rdst = load_gpr(&mut words, dst_v, X16);
                            let rsrc = load_gpr(&mut words, src_v, X17);
                            match orig_len {
                                1 => {
                                    words.push(enc::fp_access(enc::ldr(Size::B, V30, rsrc, 0)));
                                    words.push(enc::fp_access(enc::str(Size::B, V30, rdst, 0)));
                                }
                                2 => {
                                    words.push(enc::fp_access(enc::ldr(Size::H, V30, rsrc, 0)));
                                    words.push(enc::fp_access(enc::str(Size::H, V30, rdst, 0)));
                                }
                                3 => {
                                    words.push(enc::fp_access(enc::ldr(Size::H, V30, rsrc, 0)));
                                    words.push(enc::fp_access(enc::ldr(Size::B, V31, rsrc, 2)));
                                    words.push(enc::fp_access(enc::str(Size::H, V30, rdst, 0)));
                                    words.push(enc::fp_access(enc::str(Size::B, V31, rdst, 2)));
                                }
                                4 => {
                                    words.push(enc::fp_access(enc::ldr(Size::W, V30, rsrc, 0)));
                                    words.push(enc::fp_access(enc::str(Size::W, V30, rdst, 0)));
                                }
                                5..=7 => {
                                    let tail = i32_of(orig_len - 4);
                                    words.push(enc::fp_access(enc::ldr(Size::W, V30, rsrc, 0)));
                                    words.push(enc::fp_access(enc::ldur(Size::W, V31, rsrc, tail)));
                                    words.push(enc::fp_access(enc::str(Size::W, V30, rdst, 0)));
                                    words.push(enc::fp_access(enc::stur(Size::W, V31, rdst, tail)));
                                }
                                8 => {
                                    words.push(enc::fp_access(enc::ldr(Size::X, V30, rsrc, 0)));
                                    words.push(enc::fp_access(enc::str(Size::X, V30, rdst, 0)));
                                }
                                9..=15 => {
                                    let tail = i32_of(orig_len - 8);
                                    words.push(enc::fp_access(enc::ldr(Size::X, V30, rsrc, 0)));
                                    words.push(enc::fp_access(enc::ldur(Size::X, V31, rsrc, tail)));
                                    words.push(enc::fp_access(enc::str(Size::X, V30, rdst, 0)));
                                    words.push(enc::fp_access(enc::stur(Size::X, V31, rdst, tail)));
                                }
                                16 => {
                                    words.push(enc::ldr_q(V30, rsrc, 0));
                                    words.push(enc::str_q(V30, rdst, 0));
                                }
                                17..=23 | 25..=31 => {
                                    let tail = i32_of(orig_len - 16);
                                    words.push(enc::ldr_q(V30, rsrc, 0));
                                    words.push(enc::ldur_q(V31, rsrc, tail));
                                    words.push(enc::str_q(V30, rdst, 0));
                                    words.push(enc::stur_q(V31, rdst, tail));
                                }
                                24 => {
                                    words.push(enc::ldr_q(V30, rsrc, 0));
                                    words.push(enc::fp_access(enc::ldr(Size::X, V31, rsrc, 2)));
                                    words.push(enc::str_q(V30, rdst, 0));
                                    words.push(enc::fp_access(enc::str(Size::X, V31, rdst, 2)));
                                }
                                32 => {
                                    words.push(enc::ldp_q_off(V30, V31, rsrc, 0));
                                    words.push(enc::stp_q_off(V30, V31, rdst, 0));
                                }
                                _ => {}
                            }
                        }
                        inst += 1;
                        continue;
                    }
                    gpr_moves.clear();
                    const_edge_writes.clear();
                    for (k, arg_v) in [(X0, dst_v), (X1, src_v), (X2, len_v)] {
                        let dst = GprCopyLoc::Reg(k);
                        match at_loc(&reg_alloc.loc_of, to_idx(arg_v)) {
                            AllocLoc::Gpr(s) => gpr_moves.push((dst, GprCopyLoc::Reg(u32::from(s)))),
                            AllocLoc::GprSpill(s) => gpr_moves.push((dst, GprCopyLoc::Spill(s))),
                            AllocLoc::RematConst(cval) => const_edge_writes.push((dst, RematWrite::Const(cval))),
                            AllocLoc::RematGlobal(g_inst) => const_edge_writes.push((dst, RematWrite::Global(g_inst))),
                            AllocLoc::None => {
                                let cval = at_opt(&const_u64, to_idx(arg_v)).ok_or(TierError::Refused {
                                    tier: TIER,
                                    inst: Some(u32_of(inst)),
                                    what: "unallocated memmove arg",
                                })?;
                                const_edge_writes.push((dst, RematWrite::Const(cval)));
                            }
                            _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unexpected memmove arg loc" }),
                        }
                    }
                    emit_parallel_gpr_moves(&mut words, &mut gpr_moves, spill_base)?;
                    for &(dst, write) in const_edge_writes.iter() {
                        if let GprCopyLoc::Reg(r) = dst {
                            emit_remat_write_reg(&mut words, r, write);
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
                                AllocLoc::RematConst(cval) => const_edge_writes.push((dst, RematWrite::Const(cval))),
                                AllocLoc::RematGlobal(g_inst) => const_edge_writes.push((dst, RematWrite::Global(g_inst))),
                                AllocLoc::None => {
                                    let cval = at_opt(&const_u64, to_idx(arg_v)).ok_or(TierError::Refused {
                                        tier: TIER,
                                        inst: Some(u32_of(inst)),
                                        what: "unallocated memset arg",
                                    })?;
                                    const_edge_writes.push((dst, RematWrite::Const(cval)));
                                }
                                _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unexpected memset arg loc" }),
                            }
                        }
                        emit_parallel_gpr_moves(&mut words, &mut gpr_moves, spill_base)?;
                        for &(dst, write) in const_edge_writes.iter() {
                            if let GprCopyLoc::Reg(r) = dst {
                                emit_remat_write_reg(&mut words, r, write);
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
                                AllocLoc::RematConst(cval) => const_edge_writes.push((dst, RematWrite::Const(cval))),
                                AllocLoc::RematGlobal(g_inst) => const_edge_writes.push((dst, RematWrite::Global(g_inst))),
                                AllocLoc::None => {
                                    let cval = at_opt(&const_u64, to_idx(arg_v)).ok_or(TierError::Refused {
                                        tier: TIER,
                                        inst: Some(u32_of(inst)),
                                        what: "unallocated memcmp arg",
                                    })?;
                                    const_edge_writes.push((dst, RematWrite::Const(cval)));
                                }
                                _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unexpected memcmp arg loc" }),
                            }
                        }
                        emit_parallel_gpr_moves(&mut words, &mut gpr_moves, spill_base)?;
                        for &(dst, write) in const_edge_writes.iter() {
                            if let GprCopyLoc::Reg(r) = dst {
                                emit_remat_write_reg(&mut words, r, write);
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
                    } else if matches!(
                        name,
                        b"blaze.a64.crc32b"
                            | b"blaze.a64.crc32x"
                            | b"blaze.a64.crc32cb"
                            | b"blaze.a64.crc32cx"
                    ) {
                        let (c, sz) = match name {
                            b"blaze.a64.crc32b" => (false, 0u32),
                            b"blaze.a64.crc32x" => (false, 3u32),
                            b"blaze.a64.crc32cb" => (true, 0u32),
                            _ => (true, 3u32),
                        };
                        let rn = load_gpr(&mut words, at_u32(view.extra, at + 2), X16);
                        let rm = load_gpr(&mut words, at_u32(view.extra, at + 3), X17);
                        let (rd, sp) = dst_gpr(inst);
                        words.push(enc::crc32(c, sz, rd, rn, rm));
                        if let Some(s) = sp {
                            emit_st_spill_gpr(&mut words, rd, spill_base, s);
                        }
                    } else if name == b"blaze.sme2.gemm_f32"
                        || name == b"blaze.sme2.gemm_f64"
                        || name == b"blaze.sme2.gemm_i8_i32"
                        || name == b"blaze.sme2.gemm_u8_u32"
                    {
                        gpr_moves.clear();
                        const_edge_writes.clear();
                        for k in 0..7usize {
                            let arg_v = at_u32(view.extra, at + 2 + k);
                            let dst = GprCopyLoc::Reg(u32_of(k));
                            match at_loc(&reg_alloc.loc_of, to_idx(arg_v)) {
                                AllocLoc::Gpr(r) => gpr_moves.push((dst, GprCopyLoc::Reg(u32::from(r)))),
                                AllocLoc::GprSpill(s) => gpr_moves.push((dst, GprCopyLoc::Spill(s))),
                                AllocLoc::RematConst(imm) => {
                                    const_edge_writes.push((dst, RematWrite::Const(imm)));
                                }
                                AllocLoc::RematGlobal(g_inst) => {
                                    const_edge_writes.push((dst, RematWrite::Global(g_inst)));
                                }
                                _ => {
                                    let r = load_gpr(&mut words, arg_v, X16);
                                    gpr_moves.push((dst, GprCopyLoc::Reg(r)));
                                }
                            }
                        }
                        emit_parallel_gpr_moves(&mut words, &mut gpr_moves, spill_base)?;
                        for &(dst, write) in &const_edge_writes {
                            if let GprCopyLoc::Reg(r) = dst {
                                emit_remat_write_reg(&mut words, r, write);
                            }
                        }
                        let bl_w = words.len();
                        words.push(enc::bl(0));
                        if name == b"blaze.sme2.gemm_f32" {
                            sme2_f32_bl_sites.push(bl_w);
                        } else if name == b"blaze.sme2.gemm_f64" {
                            sme2_f64_bl_sites.push(bl_w);
                        } else if name == b"blaze.sme2.gemm_i8_i32" {
                            sme2_i8_i32_bl_sites.push(bl_w);
                        } else {
                            sme2_u8_u32_bl_sites.push(bl_w);
                        }
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
                        Fusion::SubsFlags => {
                            let at = to_idx(at_u32(view.data, inst));
                            let lhs = at_u32(view.extra, at);
                            let rhs = at_u32(view.extra, at + 1);
                            let w32 = bits == 32;
                            let dst = if loc == AllocLoc::None { ZR } else { rd };
                            if let Some(imm) = at_opt(&const_u64, to_idx(rhs)).filter(|&v| v <= 4095) {
                                let rl = load_gpr(&mut words, lhs, X16);
                                words.push(enc::sf(!w32, enc::subs_imm(dst, rl, trunc_u32(imm), false)));
                            } else {
                                let rl = load_gpr(&mut words, lhs, X16);
                                let rr = load_gpr(&mut words, rhs, X17);
                                words.push(enc::sf(!w32, enc::subs(dst, rl, rr)));
                            }
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
                    match at_fusion(&fusion, inst) {
                        Fusion::Bic { a, b: nb } => {
                            let ra = load_gpr(&mut words, a, X16);
                            let rb = load_gpr(&mut words, nb, X17);
                            words.push(enc::sf(!w32, enc::bic(rd, ra, rb)));
                        }
                        Fusion::Ubfx { src, lsb, width, .. } => {
                            let rs = load_gpr(&mut words, src, X16);
                            words.push(enc::ubfx(!w32, rd, rs, lsb, width));
                        }
                        Fusion::LogicalShifted { a, b: sb, kind, shift, .. } => {
                            let ra = load_gpr(&mut words, a, X16);
                            let rb = load_gpr(&mut words, sb, X17);
                            words.push(logical_shifted(!w32, 0, kind, rd, ra, rb, shift));
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
                    }
                    if let Some(s) = sp {
                        emit_st_spill_gpr(&mut words, rd, spill_base, s);
                    }
                }
                Op::Or => {
                    let (rd, sp) = dst_gpr(inst);
                    let w32 = bits == 32;
                    match at_fusion(&fusion, inst) {
                        Fusion::Orn { a, b: nb } => {
                            let ra = load_gpr(&mut words, a, X16);
                            let rb = load_gpr(&mut words, nb, X17);
                            words.push(enc::sf(!w32, enc::orn(rd, ra, rb)));
                        }
                        Fusion::RorImm { x, ror_amt, .. } => {
                            let rx = load_gpr(&mut words, x, X16);
                            words.push(if w32 {
                                enc::extr_w(rd, rx, rx, ror_amt)
                            } else {
                                enc::extr(rd, rx, rx, ror_amt)
                            });
                        }
                        Fusion::Bfxil { dst, src, lsb, width, .. } => {
                            let rdst = load_gpr(&mut words, dst, X16);
                            let rsrc = load_gpr(&mut words, src, X17);
                            if rd == rsrc && rd != rdst {
                                if rsrc != X17 {
                                    words.push(enc::mov(X17, rsrc));
                                }
                                words.push(enc::mov(rd, rdst));
                                words.push(enc::bfxil(!w32, rd, X17, lsb, width));
                            } else {
                                if rd != rdst {
                                    words.push(enc::mov(rd, rdst));
                                }
                                words.push(enc::bfxil(!w32, rd, rsrc, lsb, width));
                            }
                        }
                        Fusion::LogicalShifted { a, b: sb, kind, shift, .. } => {
                            let ra = load_gpr(&mut words, a, X16);
                            let rb = load_gpr(&mut words, sb, X17);
                            words.push(logical_shifted(!w32, 1, kind, rd, ra, rb, shift));
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
                        Fusion::RorImm { x, ror_amt, .. } => {
                            let rx = load_gpr(&mut words, x, X16);
                            words.push(if w32 {
                                enc::extr_w(rd, rx, rx, ror_amt)
                            } else {
                                enc::extr(rd, rx, rx, ror_amt)
                            });
                        }
                        Fusion::EorRor { a, b: sb, shift, .. } => {
                            let ra = load_gpr(&mut words, a, X16);
                            let rb = load_gpr(&mut words, sb, X17);
                            words.push(enc::sf(!w32, enc::eor_ror(rd, ra, rb, shift)));
                        }
                        Fusion::LogicalShifted { a, b: sb, kind, shift, .. } => {
                            let ra = load_gpr(&mut words, a, X16);
                            let rb = load_gpr(&mut words, sb, X17);
                            words.push(logical_shifted(!w32, 2, kind, rd, ra, rb, shift));
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
                                                let mid_is_call = matches!(
                                                    mop,
                                                    Op::Call
                                                        | Op::CallIndirect
                                                        | Op::Frem
                                                        | Op::TlsAddr
                                                        | Op::Udiv
                                                        | Op::Sdiv
                                                        | Op::Urem
                                                        | Op::Srem
                                                ) || (matches!(mop, Op::Memcpy | Op::Memset | Op::Memcmp)
                                                    && !at_opt(
                                                        &const_u64,
                                                        to_idx(at_u32(view.extra, to_idx(at_u32(view.data, mid)) + 2)),
                                                    )
                                                    .is_some_and(|n| n <= SOTA_BULK_INLINE_MOST))
                                                    || (mop == Op::Memmove
                                                        && small_const_memmove_len(&view, mid, &const_u64).is_none());
                                                (!is_cs && mid_is_call)
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
                        Fusion::CsincSelect { cond, a, b: bv, op_ty, rn, rm, inc_when_false, .. } => {
                            let (rd, sp) = dst_gpr(inst);
                            emit_cmp(&mut words, cond, a, bv, op_ty);
                            let r_n = load_gpr(&mut words, rn, X16);
                            let r_m = load_gpr(&mut words, rm, X17);
                            let c = if inc_when_false { cond_of(cond) } else { invert_cond(cond_of(cond)) };
                            words.push(enc::csinc(rd, r_n, r_m, c));
                            if let Some(s) = sp {
                                emit_st_spill_gpr(&mut words, rd, spill_base, s);
                            }
                            inst += 1;
                            continue;
                        }
                        Fusion::ReuseFlagsCsinc { cond, rn, rm, .. } => {
                            let (rd, sp) = dst_gpr(inst);
                            let r_n = load_gpr(&mut words, rn, X16);
                            let r_m = load_gpr(&mut words, rm, X17);
                            words.push(enc::csinc(rd, r_n, r_m, cond));
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
                        Fusion::ReuseFlagsSelect { cond } => cond,
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
                    if let Fusion::I128LoMul { lhs, rhs, lhs_src, rhs_src } = at_fusion(&fusion, inst) {
                        let a0 = match lhs_src {
                            Some(src) => load_gpr(&mut words, src, X16),
                            None => match at_loc(&reg_alloc.loc_of, to_idx(lhs)) {
                                AllocLoc::GprPair(lo, _) => u32::from(lo),
                                AllocLoc::GprPairSpill(s0, _) => {
                                    emit_ld_spill_gpr(&mut words, X16, spill_base, s0);
                                    X16
                                }
                                _ => {
                                    return Err(TierError::Refused {
                                        tier: TIER,
                                        inst: Some(u32_of(inst)),
                                        what: "unexpected i128 lo mul lhs loc",
                                    });
                                }
                            },
                        };
                        let b0 = match rhs_src {
                            Some(src) => load_gpr(&mut words, src, X17),
                            None => match at_loc(&reg_alloc.loc_of, to_idx(rhs)) {
                                AllocLoc::GprPair(lo, _) => u32::from(lo),
                                AllocLoc::GprPairSpill(s0, _) => {
                                    emit_ld_spill_gpr(&mut words, X17, spill_base, s0);
                                    X17
                                }
                                _ => {
                                    return Err(TierError::Refused {
                                        tier: TIER,
                                        inst: Some(u32_of(inst)),
                                        what: "unexpected i128 lo mul rhs loc",
                                    });
                                }
                            },
                        };
                        words.push(enc::mul(rd, a0, b0));
                        if bits < 64 {
                            words.push(enc::zext(rd, rd, bits));
                        }
                        if let Some(s) = sp {
                            emit_st_spill_gpr(&mut words, rd, spill_base, s);
                        }
                    } else {
                        let src = if let Fusion::I128HiTrunc { src: x, .. } = at_fusion(&fusion, inst) {
                            match at_loc(&reg_alloc.loc_of, to_idx(x)) {
                                AllocLoc::GprPair(_, hi) => u32::from(hi),
                                AllocLoc::GprPairSpill(_, s1) => {
                                    emit_ld_spill_gpr(&mut words, X16, spill_base, s1);
                                    X16
                                }
                                _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unexpected i128 hi trunc src loc" }),
                            }
                        } else {
                            let src_v = at_u32(view.data, inst);
                            if ty_at(view.tys, to_idx(src_v)) == Ty::I128 {
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
                            }
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
                    let mut call_regs = Regs::new(os);
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
                        let placed = place_param(&param, &mut call_regs, &mut call_nsaa).ok_or(TierError::Refused {
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
                                        AllocLoc::RematConst(cval) => const_edge_writes.push((dst, RematWrite::Const(cval))),
                                        AllocLoc::RematGlobal(g_inst) => const_edge_writes.push((dst, RematWrite::Global(g_inst))),
                                        AllocLoc::None => {
                                            let cval = at_opt(&const_u64, to_idx(arg_v)).ok_or(TierError::Refused {
                                                tier: TIER,
                                                inst: Some(u32_of(inst)),
                                                what: "unallocated non-const call arg",
                                            })?;
                                            const_edge_writes.push((dst, RematWrite::Const(cval)));
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
                    for &(dst, write) in const_edge_writes.iter() {
                        if let GprCopyLoc::Reg(r) = dst {
                            emit_remat_write_reg(&mut words, r, write);
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

                    let then_nop = then_argc == 0 || edge_is_nop(then_block, at + 3, then_argc);
                    let else_nop = else_argc == 0 || edge_is_nop(else_block, else_at + 2, else_argc);

                    if then_nop && else_nop {
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
                                emit_cond_branch_pair(&mut words, &mut patches, cond, then_block, else_block, next_block, far_branches);
                            }
                            Fusion::CmpBr { cond, a, b: bv, op_ty } => {
                                let is_zero_cmp = at_opt(&const_u64, to_idx(bv)) == Some(0)
                                    && matches!(cond, IntCond::Eq | IntCond::Ne);
                                if is_zero_cmp {
                                    let ra = load_gpr(&mut words, a, X16);
                                    if next_block == Some(to_idx(else_block)) {
                                        let kind = if cond == IntCond::Eq { PatchKind::Cbz(ra) } else { PatchKind::Cbnz(ra) };
                                        emit_branch_patch(&mut words, &mut patches, kind, then_block, far_branches);
                                    } else if next_block == Some(to_idx(then_block)) {
                                        let kind = if cond == IntCond::Eq { PatchKind::Cbnz(ra) } else { PatchKind::Cbz(ra) };
                                        emit_branch_patch(&mut words, &mut patches, kind, else_block, far_branches);
                                    } else {
                                        let kind = if cond == IntCond::Eq { PatchKind::Cbz(ra) } else { PatchKind::Cbnz(ra) };
                                        emit_branch_patch(&mut words, &mut patches, kind, then_block, far_branches);
                                        emit_branch_patch(&mut words, &mut patches, PatchKind::B, else_block, false);
                                    }
                                } else {
                                    emit_cmp(&mut words, cond, a, bv, op_ty);
                                    emit_cond_branch_pair(&mut words, &mut patches, cond_of(cond), then_block, else_block, next_block, far_branches);
                                }
                            }
                            Fusion::FcmpBr { cond, a, b: bv, double } => {
                                let va = load_fp(&mut words, a, V30);
                                let vb = load_fp(&mut words, bv, V31);
                                words.push(enc::fcmp(double, va, vb));
                                emit_cond_branch_pair(&mut words, &mut patches, cond, then_block, else_block, next_block, far_branches);
                            }
                            Fusion::BlazeResultCallBr => {
                                // Single-instruction `b.cs err_blk` directly on the callee's Carry flag (`C=1` = Err)!
                                emit_cond_branch_pair(&mut words, &mut patches, Cond::Hs, then_block, else_block, next_block, far_branches);
                            }
                            Fusion::ReuseFlagsBr { cond, .. } => {
                                emit_cond_branch_pair(&mut words, &mut patches, cond, then_block, else_block, next_block, far_branches);
                            }
                            _ => {
                                let rc = load_gpr(&mut words, cond_v, X16);
                                if next_block == Some(to_idx(else_block)) {
                                    emit_branch_patch(&mut words, &mut patches, PatchKind::Cbnz(rc), then_block, far_branches);
                                } else if next_block == Some(to_idx(then_block)) {
                                    emit_branch_patch(&mut words, &mut patches, PatchKind::Cbz(rc), else_block, far_branches);
                                } else {
                                    emit_branch_patch(&mut words, &mut patches, PatchKind::Cbnz(rc), then_block, far_branches);
                                    emit_branch_patch(&mut words, &mut patches, PatchKind::B, else_block, false);
                                }
                            }
                        }
                    } else {
                        let emit_br_cond = |words: &mut Vec<u32, Alloc<'a>>, branch_if_true: bool| -> (usize, PatchKind) {
                            match at_fusion(&fusion, inst) {
                                Fusion::CmpBr { cond, a, b: bv, op_ty } => {
                                    if at_opt(&const_u64, to_idx(bv)) == Some(0) && matches!(cond, IntCond::Eq | IntCond::Ne) {
                                        let ra = load_gpr(words, a, X16);
                                        let w = words.len();
                                        let jump_on_zero = (cond == IntCond::Eq) == branch_if_true;
                                        let k = if jump_on_zero {
                                            words.push(enc::cbz(ra, 0));
                                            PatchKind::Cbz(ra)
                                        } else {
                                            words.push(enc::cbnz(ra, 0));
                                            PatchKind::Cbnz(ra)
                                        };
                                        (w, k)
                                    } else {
                                        emit_cmp(words, cond, a, bv, op_ty);
                                        let c = if branch_if_true { cond_of(cond) } else { invert_cond(cond_of(cond)) };
                                        let w = words.len();
                                        words.push(enc::b_cond(c, 0));
                                        (w, PatchKind::BCond(c))
                                    }
                                }
                                Fusion::FcmpBr { cond, a, b: bv, double } => {
                                    let va = load_fp(words, a, V30);
                                    let vb = load_fp(words, bv, V31);
                                    words.push(enc::fcmp(double, va, vb));
                                    let c = if branch_if_true { cond } else { invert_cond(cond) };
                                    let w = words.len();
                                    words.push(enc::b_cond(c, 0));
                                    (w, PatchKind::BCond(c))
                                }
                                Fusion::BlazeResultCallBr => {
                                    let c = if branch_if_true { Cond::Hs } else { Cond::Lo };
                                    let w = words.len();
                                    words.push(enc::b_cond(c, 0));
                                    (w, PatchKind::BCond(c))
                                }
                                Fusion::ReuseFlagsBr { cond, .. } => {
                                    let c = if branch_if_true { cond } else { invert_cond(cond) };
                                    let w = words.len();
                                    words.push(enc::b_cond(c, 0));
                                    (w, PatchKind::BCond(c))
                                }
                                _ => {
                                    let rc = load_gpr(words, cond_v, X16);
                                    let w = words.len();
                                    let k = if branch_if_true {
                                        words.push(enc::cbnz(rc, 0));
                                        PatchKind::Cbnz(rc)
                                    } else {
                                        words.push(enc::cbz(rc, 0));
                                        PatchKind::Cbz(rc)
                                    };
                                    (w, k)
                                }
                            }
                        };
                        if then_nop && !else_nop {
                            if next_block == Some(to_idx(then_block)) && len < 32_768 {
                                // Hot `then` falls straight through into `next_block` with ZERO moves;
                                // sink cold `else` edge moves to a deferred stub at the end of the function!
                                let (widx, kind) = emit_br_cond(&mut words, false);
                                cold_stubs.push(ColdEdgeStub {
                                    branch_word_idx: widx,
                                    branch_kind: kind,
                                    target_block: else_block,
                                    args_start: else_at + 2,
                                    args_len: else_argc,
                                });
                            } else {
                                // Zero-move `then` arm (e.g. rotated-loop back-edge): branch DIRECTLY to
                                // `then_block` with ZERO trampoline, falling through into cold `else` moves!
                                let (widx, kind) = emit_br_cond(&mut words, true);
                                if far_branches {
                                    words.pop();
                                    emit_branch_patch(&mut words, &mut patches, kind, then_block, true);
                                } else {
                                    patches.push(BranchPatch { word_idx: widx, kind, target_block: then_block });
                                }
                                emit_edge_moves(&mut words, &mut gpr_moves, &mut fp_moves, &mut const_edge_writes, else_block, else_at + 2, else_argc);
                                if next_block != Some(to_idx(else_block)) {
                                    let w_else = words.len();
                                    words.push(enc::b(0));
                                    patches.push(BranchPatch { word_idx: w_else, kind: PatchKind::B, target_block: else_block });
                                }
                            }
                        } else if !then_nop && else_nop {
                            if next_block == Some(to_idx(else_block)) && len < 32_768 {
                                // Hot `else` falls straight through into `next_block` with ZERO moves;
                                // sink cold `then` edge moves to a deferred stub at the end of the function!
                                let (widx, kind) = emit_br_cond(&mut words, true);
                                cold_stubs.push(ColdEdgeStub {
                                    branch_word_idx: widx,
                                    branch_kind: kind,
                                    target_block: then_block,
                                    args_start: at + 3,
                                    args_len: then_argc,
                                });
                            } else {
                                // Zero-move `else` arm (e.g. inverted rotated-loop back-edge `wyhash_stream`):
                                // branch DIRECTLY to `else_block` with ZERO trampoline, falling through into
                                // cold `then` moves (and falling straight into `then_block` when next)!
                                let (widx, kind) = emit_br_cond(&mut words, false);
                                if far_branches {
                                    words.pop();
                                    emit_branch_patch(&mut words, &mut patches, kind, else_block, true);
                                } else {
                                    patches.push(BranchPatch { word_idx: widx, kind, target_block: else_block });
                                }
                                emit_edge_moves(&mut words, &mut gpr_moves, &mut fp_moves, &mut const_edge_writes, then_block, at + 3, then_argc);
                                if next_block != Some(to_idx(then_block)) {
                                    let w_then = words.len();
                                    words.push(enc::b(0));
                                    patches.push(BranchPatch { word_idx: w_then, kind: PatchKind::B, target_block: then_block });
                                }
                            }
                        } else {
                            let skip_then = emit_br_cond(&mut words, false);
                            emit_edge_moves(&mut words, &mut gpr_moves, &mut fp_moves, &mut const_edge_writes, then_block, at + 3, then_argc);
                            let w_then = words.len();
                            words.push(enc::b(0));
                            patches.push(BranchPatch { word_idx: w_then, kind: PatchKind::B, target_block: then_block });

                            let else_start = words.len();
                            let delta = i32_of(else_start - skip_then.0);
                            let skip_word = match skip_then.1 {
                                PatchKind::BCond(inv) => enc::b_cond(inv, delta),
                                PatchKind::Cbz(rc) => enc::cbz(rc, delta),
                                PatchKind::Cbnz(rc) => enc::cbnz(rc, delta),
                                _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unexpected skip_then patch kind" }),
                            };
                            set_at(&mut words, skip_then.0, skip_word);
                            emit_edge_moves(&mut words, &mut gpr_moves, &mut fp_moves, &mut const_edge_writes, else_block, else_at + 2, else_argc);
                            if next_block != Some(to_idx(else_block)) {
                                let w_else = words.len();
                                words.push(enc::b(0));
                                patches.push(BranchPatch { word_idx: w_else, kind: PatchKind::B, target_block: else_block });
                            }
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
                    let scrut_ty = ty_at(view.tys, to_idx(scrutinee));
                    let width_words = to_idx(scrut_ty.const_words());

                    if scrut_ty == Ty::I128 {
                        if width_words != 4 {
                            return Err(TierError::Refused {
                                tier: TIER,
                                inst: Some(u32_of(inst)),
                                what: "non-4-word i128 switch",
                            });
                        }
                        let load_h = |words: &mut Vec<u32, Alloc<'a>>, hi: bool| -> Reg {
                            match at_loc(&reg_alloc.loc_of, to_idx(scrutinee)) {
                                AllocLoc::GprPair(lo, h) => u32::from(if hi { h } else { lo }),
                                AllocLoc::GprPairSpill(s0, s1) => {
                                    emit_ld_spill_gpr(words, X16, spill_base, if hi { s1 } else { s0 });
                                    X16
                                }
                                _ => {
                                    internal_err.set(Some(TierError::Refused {
                                        tier: TIER,
                                        inst: Some(scrutinee),
                                        what: "unexpected i128 switch scrutinee loc",
                                    }));
                                    X16
                                }
                            }
                        };
                        let mut pos = cases_at + 1;
                        for _ in 0..num_cases {
                            let lo = u64::from(at_u32(view.extra, pos)) | (u64::from(at_u32(view.extra, pos + 1)) << 32);
                            let hi = u64::from(at_u32(view.extra, pos + 2)) | (u64::from(at_u32(view.extra, pos + 3)) << 32);
                            pos += 4;
                            let case_block = at_u32(view.extra, pos);
                            let case_argc = to_idx(at_u32(view.extra, pos + 1));
                            let case_args_start = pos + 2;
                            pos += 2 + case_argc;

                            let sr_lo = load_h(&mut words, false);
                            if lo <= 4095 {
                                words.push(enc::cmp_imm(sr_lo, trunc_u32(lo)));
                            } else {
                                emit_const_u64(&mut words, X17, lo);
                                words.push(enc::cmp(sr_lo, X17));
                            }
                            let skip_lo = words.len();
                            words.push(enc::b_cond(Cond::Ne, 0));

                            let sr_hi = load_h(&mut words, true);
                            if hi <= 4095 {
                                words.push(enc::cmp_imm(sr_hi, trunc_u32(hi)));
                            } else {
                                emit_const_u64(&mut words, X17, hi);
                                words.push(enc::cmp(sr_hi, X17));
                            }
                            if case_argc == 0 {
                                emit_branch_patch(&mut words, &mut patches, PatchKind::BCond(Cond::Eq), case_block, far_branches);
                                let after = words.len();
                                set_at(&mut words, skip_lo, enc::b_cond(Cond::Ne, i32_of(after - skip_lo)));
                            } else {
                                let skip_hi = words.len();
                                words.push(enc::b_cond(Cond::Ne, 0));
                                emit_edge_moves(&mut words, &mut gpr_moves, &mut fp_moves, &mut const_edge_writes, case_block, case_args_start, case_argc);
                                let w_jump = words.len();
                                words.push(enc::b(0));
                                patches.push(BranchPatch { word_idx: w_jump, kind: PatchKind::B, target_block: case_block });
                                let after = words.len();
                                set_at(&mut words, skip_lo, enc::b_cond(Cond::Ne, i32_of(after - skip_lo)));
                                set_at(&mut words, skip_hi, enc::b_cond(Cond::Ne, i32_of(after - skip_hi)));
                            }
                        }
                        emit_edge_moves(&mut words, &mut gpr_moves, &mut fp_moves, &mut const_edge_writes, default_block, at + 3, default_argc);
                        let def_range = view.block(Block(default_block));
                        if default_argc == 0
                            && !def_range.is_empty()
                            && view.op(def_range.start) == Op::Unreachable
                            && sole_switch_default.get(to_idx(default_block)).copied().unwrap_or(blaze_lir::func::NONE) != blaze_lir::func::NONE
                        {
                            let sr_lo = load_h(&mut words, false);
                            if sr_lo != X9 {
                                words.push(enc::mov(X9, sr_lo));
                            }
                        }
                        if next_block != Some(to_idx(default_block)) {
                            let widx = words.len();
                            words.push(enc::b(0));
                            patches.push(BranchPatch { word_idx: widx, kind: PatchKind::B, target_block: default_block });
                        }
                        inst += 1;
                        continue;
                    }

                    let sr = load_gpr(&mut words, scrutinee, X16);

                    // Dense O(1) PC-relative jump table (`adr` + `ldrsw [tab, idx, lsl #2]` + `add` + `br`),
                    // sharing out-of-line tables across zero-move `taildup` dispatch sites for peak I-cache/L1D density!
                    if !far_branches
                        && let Some((min_v, span)) = dense_switch_plan(&view, inst)
                    {
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
                        // Use `sr` directly when `min_v == 0` (eliminates wasted `mov x16, sr`!).
                        let idx_reg = if min_v == 0 && sr != X17 {
                            sr
                        } else if min_v == 0 {
                            words.push(enc::mov(X16, sr));
                            X16
                        } else if min_v <= 4095 {
                            words.push(enc::sub_imm(X16, sr, trunc_u32(min_v), false));
                            X16
                        } else {
                            emit_const_u64(&mut words, X17, min_v);
                            words.push(enc::sub(X16, sr, X17));
                            X16
                        };
                        // Exhaustive enum niche switch: if `default_block` is `Op::Unreachable`-only,
                        // skip the `cmp + b.hi` bounds check completely (saves 2 ops + 1 BTB entry)!
                        let default_is_unreachable = default_argc == 0
                            && view
                                .block(Block(default_block))
                                .find(|&i| view.op(i) != Op::Param)
                                .is_some_and(|i| view.op(i) == Op::Unreachable);
                        let default_nop = default_is_unreachable
                            || default_argc == 0
                            || edge_is_nop(default_block, at + 3, default_argc);
                        let all_entries_nop = entries
                            .iter()
                            .all(|&(tb, astart, argc)| argc == 0 || edge_is_nop(tb, astart, argc));
                        let share_table = all_entries_nop && len < 32_768;

                        let b_def = if default_is_unreachable {
                            None
                        } else {
                            words.push(enc::cmp_imm(idx_reg, u32_of(span - 1)));
                            let idx = words.len();
                            words.push(enc::b_cond(Cond::Hi, 0));
                            if default_nop {
                                patches.push(BranchPatch {
                                    word_idx: idx,
                                    kind: PatchKind::BCond(Cond::Hi),
                                    target_block: default_block,
                                });
                            } else if share_table {
                                cold_stubs.push(ColdEdgeStub {
                                    branch_word_idx: idx,
                                    branch_kind: PatchKind::BCond(Cond::Hi),
                                    target_block: default_block,
                                    args_start: at + 3,
                                    args_len: default_argc,
                                });
                            }
                            Some(idx)
                        };
                        if share_table {
                            // Out-of-line shared jump table at function tail: keeps hot arms tiny & L1D-hot!
                            let adr_word_idx = words.len();
                            words.push(enc::adr(X17, 0));
                            words.push(enc::ldrsw_reg_scaled(X16, X17, idx_reg));
                            words.push(enc::add(X17, X17, X16));
                            words.push(enc::br(X17));
                            let mut found = false;
                            for tab in shared_jump_tables.iter_mut() {
                                if tab.targets.len() == span
                                    && tab.targets.iter().zip(entries.iter()).all(|(&t, &(tb, _, _))| t == tb)
                                {
                                    tab.adr_sites.push(adr_word_idx);
                                    found = true;
                                    break;
                                }
                            }
                            if !found {
                                let mut targets: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(span, alloc);
                                for &(tb, _, _) in entries.iter() {
                                    targets.push(tb);
                                }
                                let mut adr_sites: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(4, alloc);
                                adr_sites.push(adr_word_idx);
                                shared_jump_tables.push(SharedJumpTable { targets, adr_sites });
                            }
                            inst += 1;
                            continue;
                        }
                        // Inline fallback for switches whose case entries require edge-move trampolines.
                        // `adr x17, table` (4 instructions ahead = +16 bytes).
                        words.push(enc::adr(X17, 16));
                        words.push(enc::ldrsw_reg_scaled(X16, X17, idx_reg));
                        words.push(enc::add(X17, X17, X16));
                        words.push(enc::br(X17));
                        let table_start_word = words.len();
                        for _ in 0..span {
                            words.push(0);
                        }
                        // Emit default trampoline once only when default actually requires edge moves.
                        let mut def_tramp_word: Option<usize> = None;
                        if !default_nop {
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
                            if argc == 0 || edge_is_nop(tb, astart, argc) {
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
                                let wb = words.len();
                                words.push(enc::b(0));
                                patches.push(BranchPatch { word_idx: wb, kind: PatchKind::B, target_block: tb });
                                set_at(&mut words, eidx, word_delta(tw, table_start_word).wrapping_mul(4).cast_unsigned());
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
                            emit_branch_patch(&mut words, &mut patches, PatchKind::BCond(Cond::Eq), case_block, far_branches);
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
                    let def_range = view.block(Block(default_block));
                    if default_argc == 0
                        && !def_range.is_empty()
                        && view.op(def_range.start) == Op::Unreachable
                        && sole_switch_default.get(to_idx(default_block)).copied().unwrap_or(blaze_lir::func::NONE) != blaze_lir::func::NONE
                        && sr != X9
                    {
                        words.push(enc::mov(X9, sr));
                    }
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
                            AllocLoc::RematGlobal(g_inst) => emit_remat_global(&mut words, X0, g_inst),
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
                                AllocLoc::RematConst(cval) => const_edge_writes.push((dst, RematWrite::Const(cval))),
                                AllocLoc::RematGlobal(g_inst) => const_edge_writes.push((dst, RematWrite::Global(g_inst))),
                                AllocLoc::None => {
                                    let cval = at_opt(&const_u64, to_idx(rv)).ok_or(TierError::Refused {
                                        tier: TIER,
                                        inst: Some(u32_of(inst)),
                                        what: "unallocated non-const 2-word return",
                                    })?;
                                    const_edge_writes.push((dst, RematWrite::Const(cval)));
                                }
                                _ => return Err(TierError::Refused { tier: TIER, inst: Some(u32_of(inst)), what: "unexpected 2-word return loc" }),
                            }
                        }
                        emit_parallel_gpr_moves(&mut words, &mut gpr_moves, spill_base)?;
                        for &(dst, write) in const_edge_writes.iter() {
                            if let GprCopyLoc::Reg(r) = dst {
                                emit_remat_write_reg(&mut words, r, write);
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
                                AllocLoc::RematGlobal(g_inst) => emit_remat_global(&mut words, X0, g_inst),
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
                        let blk = Block(u32_of(b));
                        let switch_term = sole_switch_default.get(b).copied().unwrap_or(blaze_lir::func::NONE);
                        let held = if view.block(blk).start == inst
                            && switch_term != blaze_lir::func::NONE
                            && (far_branches || dense_switch_plan(&view, to_idx(switch_term)).is_none())
                        {
                            Held::Reg(9)
                        } else {
                            Held::Unknown
                        };
                        out.traps.push(TrapRow {
                            at: u32_of(words.len() * 4),
                            kind: TrapKind::Intrinsic,
                            site: Some((0, ord)),
                            held,
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
    let encode_branch_patch = |kind: PatchKind, delta: i32| -> Result<u32, TierError> {
        let fits_signed = |val: i32, bits: u32| -> bool {
            let min = -(1i64 << (bits - 1));
            let max = (1i64 << (bits - 1)) - 1;
            let v = i64::from(val);
            v >= min && v <= max
        };
        let too_far = || TierError::TooLarge { tier: TIER, what: "branch target out of reach" };
        match kind {
            PatchKind::B => {
                if !fits_signed(delta, 26) {
                    return Err(too_far());
                }
                Ok(enc::b(delta))
            }
            PatchKind::BCond(c) => {
                if !fits_signed(delta, 19) {
                    return Err(too_far());
                }
                Ok(enc::b_cond(c, delta))
            }
            PatchKind::Cbz(r) => {
                if !fits_signed(delta, 19) {
                    return Err(too_far());
                }
                Ok(enc::cbz(r, delta))
            }
            PatchKind::Cbnz(r) => {
                if !fits_signed(delta, 19) {
                    return Err(too_far());
                }
                Ok(enc::cbnz(r, delta))
            }
            PatchKind::Tbz(r, bit) => {
                if !fits_signed(delta, 14) {
                    return Err(too_far());
                }
                Ok(enc::tbz(r, bit, delta))
            }
            PatchKind::Tbnz(r, bit) => {
                if !fits_signed(delta, 14) {
                    return Err(too_far());
                }
                Ok(enc::tbnz(r, bit, delta))
            }
            PatchKind::JumpTableEntry { .. } => {
                Err(TierError::Refused { tier: TIER, inst: None, what: "unexpected jump table patch kind" })
            }
        }
    };

    // 4b. Emit deferred cold edge-move stubs at the tail of the function (out of hot I-cache lines).
    for stub in &cold_stubs {
        let stub_w = words.len();
        let delta = word_delta(stub_w, stub.branch_word_idx);
        set_at(&mut words, stub.branch_word_idx, encode_branch_patch(stub.branch_kind, delta)?);
        emit_edge_moves(
            &mut words,
            &mut gpr_moves,
            &mut fp_moves,
            &mut const_edge_writes,
            stub.target_block,
            stub.args_start,
            stub.args_len,
        );
        let wb = words.len();
        words.push(enc::b(0));
        patches.push(BranchPatch { word_idx: wb, kind: PatchKind::B, target_block: stub.target_block });
    }
    if let Some(err) = internal_err.get() {
        return Err(err);
    }

    // 4c. Emit shared out-of-line PC-relative jump tables at the tail of the function.
    for tab in &shared_jump_tables {
        let table_start_word = words.len();
        for &adr_w in &tab.adr_sites {
            let byte_delta = word_delta(table_start_word, adr_w).wrapping_mul(4);
            let bd = i64::from(byte_delta);
            if !(-(1i64 << 20)..(1i64 << 20)).contains(&bd) {
                return Err(TierError::TooLarge { tier: TIER, what: "jump-table adr target out of reach" });
            }
            set_at(&mut words, adr_w, enc::adr(X17, byte_delta));
        }
        for &tb in &tab.targets {
            let eidx = words.len();
            words.push(0);
            patches.push(BranchPatch {
                word_idx: eidx,
                kind: PatchKind::JumpTableEntry { table_start_word },
                target_block: tb,
            });
        }
    }
    // 4d. Emit local PC-relative SME2 `FMOPA` micro-kernel stubs once per function when invoked.
    if !sme2_f32_bl_sites.is_empty() {
        while words.len() % 4 != 0 {
            words.push(enc::nop());
        }
        let stub_w = words.len();
        let stub_words = super::sme2::emit_sme2_gemm_f32_words(alloc);
        words.extend_from_slice(&stub_words);
        for &bl_w in &sme2_f32_bl_sites {
            set_at(&mut words, bl_w, enc::bl(word_delta(stub_w, bl_w)));
        }
    }
    if !sme2_f64_bl_sites.is_empty() {
        while words.len() % 4 != 0 {
            words.push(enc::nop());
        }
        let stub_w = words.len();
        let stub_words = super::sme2::emit_sme2_gemm_f64_words(alloc);
        words.extend_from_slice(&stub_words);
        for &bl_w in &sme2_f64_bl_sites {
            set_at(&mut words, bl_w, enc::bl(word_delta(stub_w, bl_w)));
        }
    }
    if !sme2_i8_i32_bl_sites.is_empty() {
        while words.len() % 4 != 0 {
            words.push(enc::nop());
        }
        let stub_w = words.len();
        let stub_words = super::sme2::emit_sme2_gemm_i8_i32_words(alloc);
        words.extend_from_slice(&stub_words);
        for &bl_w in &sme2_i8_i32_bl_sites {
            set_at(&mut words, bl_w, enc::bl(word_delta(stub_w, bl_w)));
        }
    }
    if !sme2_u8_u32_bl_sites.is_empty() {
        while words.len() % 4 != 0 {
            words.push(enc::nop());
        }
        let stub_w = words.len();
        let stub_words = super::sme2::emit_sme2_gemm_u8_u32_words(alloc);
        words.extend_from_slice(&stub_words);
        for &bl_w in &sme2_u8_u32_bl_sites {
            set_at(&mut words, bl_w, enc::bl(word_delta(stub_w, bl_w)));
        }
    }

    // 5. Patch all intra-function branch displacements & jump table entries.
    for p in &patches {
        let target_w = at_usize(&block_word_off, to_idx(p.target_block));
        let delta = word_delta(target_w, p.word_idx);
        let word = match p.kind {
            PatchKind::JumpTableEntry { table_start_word } => {
                word_delta(target_w, table_start_word).wrapping_mul(4).cast_unsigned()
            }
            other => encode_branch_patch(other, delta)?,
        };
        set_at(&mut words, p.word_idx, word);
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

    // 6. Drain deferred remat-global relocations and encode AArch64 words little-endian into `Emitted.code`.
    for rel in remat_relocs.into_inner() {
        out.relocs.push(rel);
    }
    out.code.reserve(words.len() * 4);
    for &w in &words {
        out.code.extend_from_slice(&w.to_le_bytes());
    }
    Ok((out, out_clobbers))
}

#[inline]
fn emit_branch_patch<'a>(
    words: &mut Vec<u32, Alloc<'a>>,
    patches: &mut Vec<BranchPatch, Alloc<'a>>,
    kind: PatchKind,
    target_block: u32,
    far_branches: bool,
) {
    if far_branches {
        match kind {
            PatchKind::BCond(c) => {
                words.push(enc::b_cond(invert_cond(c), 2));
                let wb = words.len();
                words.push(enc::b(0));
                patches.push(BranchPatch { word_idx: wb, kind: PatchKind::B, target_block });
                return;
            }
            PatchKind::Cbz(r) => {
                words.push(enc::cbnz(r, 2));
                let wb = words.len();
                words.push(enc::b(0));
                patches.push(BranchPatch { word_idx: wb, kind: PatchKind::B, target_block });
                return;
            }
            PatchKind::Cbnz(r) => {
                words.push(enc::cbz(r, 2));
                let wb = words.len();
                words.push(enc::b(0));
                patches.push(BranchPatch { word_idx: wb, kind: PatchKind::B, target_block });
                return;
            }
            _ => {}
        }
    }
    let widx = words.len();
    words.push(match kind {
        PatchKind::B => enc::b(0),
        PatchKind::BCond(c) => enc::b_cond(c, 0),
        PatchKind::Cbz(r) => enc::cbz(r, 0),
        PatchKind::Cbnz(r) => enc::cbnz(r, 0),
        PatchKind::Tbz(r, bit) => enc::tbz(r, bit, 0),
        PatchKind::Tbnz(r, bit) => enc::tbnz(r, bit, 0),
        PatchKind::JumpTableEntry { .. } => 0,
    });
    patches.push(BranchPatch { word_idx: widx, kind, target_block });
}

#[inline]
fn emit_cond_branch_pair<'a>(
    words: &mut Vec<u32, Alloc<'a>>,
    patches: &mut Vec<BranchPatch, Alloc<'a>>,
    cond: Cond,
    then_block: u32,
    else_block: u32,
    next_block: Option<usize>,
    far_branches: bool,
) {
    if next_block == Some(to_idx(else_block)) {
        emit_branch_patch(words, patches, PatchKind::BCond(cond), then_block, far_branches);
    } else if next_block == Some(to_idx(then_block)) {
        emit_branch_patch(words, patches, PatchKind::BCond(invert_cond(cond)), else_block, far_branches);
    } else {
        emit_branch_patch(words, patches, PatchKind::BCond(cond), then_block, far_branches);
        emit_branch_patch(words, patches, PatchKind::B, else_block, false);
    }
}

#[cfg(test)]
mod tests {
    use blaze_lir::{
        Flags, IntCond, Op, Scalar, Ty,
        builder::{Builder, Full, MemAttrs, Param, Variadic},
        func::{BlockKind, Conv},
    };
    use blaze_own::{Alloc, Vec};
    use blaze_tier::{Emitted, Machine, Names, Outside, RoutinePlaces};

    use super::{CallMode, emit_sota_a64};

    struct NoNames;
    impl Names for NoNames {
        fn name(&self, _of: Outside) -> Option<blaze_tier::Named<'_>> {
            None
        }

        fn routines(&self) -> RoutinePlaces {
            RoutinePlaces::NONE
        }
    }

    fn test_alloc() -> Alloc<'static> {
        blaze_own::testing::heap().alloc()
    }

    fn decode_words(code: &[u8], alloc: Alloc<'static>) -> Vec<u32, Alloc<'static>> {
        assert_eq!(code.len() % 4, 0, "AArch64 code must be 4-byte aligned");
        let mut words: Vec<u32, Alloc<'static>> = Vec::with_capacity_in(code.len() / 4, alloc);
        for chunk in code.chunks_exact(4) {
            words.push(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
        }
        words
    }

    #[inline]
    fn is_mul_x(w: u32) -> bool {
        (w & 0xffe0_fc00) == 0x9b00_7c00
    }

    #[inline]
    fn is_madd_x(w: u32) -> bool {
        (w & 0xffe0_8000) == 0x9b00_0000 && ((w >> 10) & 0x1f) != 31
    }

    #[inline]
    fn is_umulh(w: u32) -> bool {
        (w & 0xffe0_fc00) == 0x9bc0_7c00
    }

    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    fn exec_leaf(emitted: Emitted<'static>) -> blaze_inproc::exec::Executable {
        use blaze_code::image::ImageError;
        use blaze_code::place::{At, PlaceError, Words};
        use blaze_code::record::{Symbol, Target};
        use blaze_tier::record::{encode, place, read, seal};

        struct NoWords;
        impl Words for NoWords {
            fn word(&mut self, target: Target, _call: bool) -> Result<u64, ImageError> {
                Err(ImageError::Unbound { target })
            }

            fn value(&mut self, target: Target) -> Result<u64, ImageError> {
                Err(ImageError::Unbound { target })
            }
        }

        let sealed = seal(emitted).expect("emitted record must seal cleanly");
        let mut bytes: Vec<u8, Alloc<'static>> = Vec::new_in(test_alloc());
        encode(&sealed, &mut bytes);
        let view = read(&bytes).expect("encoded record must read back");
        let code_bytes = view.code().len();
        let sym = Symbol::of_name("blaze-emit sota isel unit test");
        let mut done: Result<(), PlaceError> = Ok(());
        let exec = blaze_inproc::exec::Executable::written(code_bytes, 64, |code, data| {
            let at = At { code: code.as_ptr() as u64, relro: data.as_ptr() as u64 };
            done = place(&view, sym, at, &mut NoWords, code, &mut []);
        })
        .expect("W^X exec mapping must succeed");
        done.expect("place must succeed");
        exec
    }

    #[test]
    fn widening_u128_mul_from_zext_u64_emits_no_madd() {
        let alloc = test_alloc();

        // 1. wyhash mix kernel: `let p = (a as u128).wrapping_mul(b as u128); (p as u64) ^ ((p >> 64) as u64)`
        let wymix_emitted = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let a = b.param(Ty::I64);
            let bv = b.param(Ty::I64);
            let za = b.convert(Op::Zext, Flags::NONE, Ty::I128, a);
            let zb = b.convert(Op::Zext, Flags::NONE, Ty::I128, bv);
            let prod = b.binary(Op::Mul, Flags::NONE, za, zb);
            let lo = b.convert(Op::Trunc, Flags::NONE, Ty::I64, prod);
            let c64 = b.iconst(Ty::I128, 64);
            let sh = b.binary(Op::Lshr, Flags::NONE, prod, c64);
            let hi = b.convert(Op::Trunc, Flags::NONE, Ty::I64, sh);
            let mixed = b.binary(Op::Xor, Flags::NONE, lo, hi);
            b.ret(&[mixed]);
            let func = b.finish_checked().expect("wymix LIR builds");
            emit_sota_a64(func.view(), &NoNames, CallMode::DirectBl, Machine::LSE, true, alloc)
                .expect("wymix emits")
        };
        let wymix_words = decode_words(&wymix_emitted.code, alloc);
        assert_eq!(wymix_words.iter().filter(|&&w| is_mul_x(w)).count(), 1, "expected 1 mul");
        assert_eq!(wymix_words.iter().filter(|&&w| is_umulh(w)).count(), 1, "expected 1 umulh");
        assert_eq!(
            wymix_words.iter().filter(|&&w| is_madd_x(w)).count(),
            0,
            "widening Zext(u64 -> u128) * Zext(u64 -> u128) must emit ZERO madd"
        );
        let shift_count = wymix_words
            .iter()
            .filter(|&&w| {
                (w & 0x7f80_0000) == 0x5300_0000
                    || (w & 0xffe0_fc00) == 0x9ac0_2000
                    || (w & 0xffe0_fc00) == 0x9ac0_2400
            })
            .count();
        assert_eq!(shift_count, 0, "(prod >> 64) as u64 must read prod_hi directly with ZERO shifts");

        // 2. Direct widening `(a as u128).wrapping_mul(b as u128) -> u128`.
        let wide_emitted = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I128)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let a = b.param(Ty::I64);
            let bv = b.param(Ty::I64);
            let za = b.convert(Op::Zext, Flags::NONE, Ty::I128, a);
            let zb = b.convert(Op::Zext, Flags::NONE, Ty::I128, bv);
            let prod = b.binary(Op::Mul, Flags::NONE, za, zb);
            b.ret(&[prod]);
            let func = b.finish_checked().expect("wide u128 mul LIR builds");
            emit_sota_a64(func.view(), &NoNames, CallMode::DirectBl, Machine::LSE, true, alloc)
                .expect("wide u128 mul emits")
        };
        let wide_words = decode_words(&wide_emitted.code, alloc);
        assert_eq!(wide_words.iter().filter(|&&w| is_mul_x(w)).count(), 1);
        assert_eq!(wide_words.iter().filter(|&&w| is_umulh(w)).count(), 1);
        assert_eq!(wide_words.iter().filter(|&&w| is_madd_x(w)).count(), 0);

        // 3. One-sided widening `a_u128.wrapping_mul(b as u128)` -> exactly 1 madd (skips lhs_lo * rhs_hi).
        let half_emitted = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I128), Param::of(Ty::I64)],
                &[Param::of(Ty::I128)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let a = b.param(Ty::I128);
            let bv = b.param(Ty::I64);
            let zb = b.convert(Op::Zext, Flags::NONE, Ty::I128, bv);
            let prod = b.binary(Op::Mul, Flags::NONE, a, zb);
            b.ret(&[prod]);
            let func = b.finish_checked().expect("half-wide u128 mul LIR builds");
            emit_sota_a64(func.view(), &NoNames, CallMode::DirectBl, Machine::LSE, true, alloc)
                .expect("half-wide u128 mul emits")
        };
        let half_words = decode_words(&half_emitted.code, alloc);
        assert_eq!(half_words.iter().filter(|&&w| is_mul_x(w)).count(), 1);
        assert_eq!(half_words.iter().filter(|&&w| is_umulh(w)).count(), 1);
        assert_eq!(
            half_words.iter().filter(|&&w| is_madd_x(w)).count(),
            1,
            "one-sided zero-high u128 mul must emit exactly 1 madd"
        );

        // 4. `Zext(u64 -> u128) * IconstWide(hi == 0)` -> ZERO madd.
        let const_emitted = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::I64)], &[Param::of(Ty::I128)]);
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let a = b.param(Ty::I64);
            let za = b.convert(Op::Zext, Flags::NONE, Ty::I128, a);
            let k = b.iconst(Ty::I128, 0xa076_1d64_78bd_642fu128);
            let prod = b.binary(Op::Mul, Flags::NONE, za, k);
            b.ret(&[prod]);
            let func = b.finish_checked().expect("const-wide u128 mul LIR builds");
            emit_sota_a64(func.view(), &NoNames, CallMode::DirectBl, Machine::LSE, true, alloc)
                .expect("const-wide u128 mul emits")
        };
        let const_words = decode_words(&const_emitted.code, alloc);
        assert_eq!(const_words.iter().filter(|&&w| is_mul_x(w)).count(), 1);
        assert_eq!(const_words.iter().filter(|&&w| is_umulh(w)).count(), 1);
        assert_eq!(const_words.iter().filter(|&&w| is_madd_x(w)).count(), 0);

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let ex_mix = exec_leaf(wymix_emitted);
            let f_mix: extern "C" fn(u64, u64) -> u64 = unsafe { ex_mix.as_fn() };
            let ex_wide = exec_leaf(wide_emitted);
            let f_wide: extern "C" fn(u64, u64) -> u128 = unsafe { ex_wide.as_fn() };
            let ex_half = exec_leaf(half_emitted);
            let f_half: extern "C" fn(u128, u64) -> u128 = unsafe { ex_half.as_fn() };
            let ex_const = exec_leaf(const_emitted);
            let f_const: extern "C" fn(u64) -> u128 = unsafe { ex_const.as_fn() };

            let cases: &[(u64, u64)] = &[
                (0, 0),
                (1, 0),
                (1, 1),
                (u64::MAX, u64::MAX),
                (u64::MAX, 2),
                (0xa076_1d64_78bd_642f, 0xe703_7ed1_a0b4_28db),
                (0x0123_4567_89ab_cdef, 0xfedc_ba98_7654_3210),
            ];
            for &(a, bv) in cases {
                let want_p = u128::from(a).wrapping_mul(u128::from(bv));
                let want_mix = (want_p as u64) ^ ((want_p >> 64) as u64);
                assert_eq!(f_mix(a, bv), want_mix, "wymix({a:#x}, {bv:#x}) mismatch");
                assert_eq!(f_wide(a, bv), want_p, "wide_mul({a:#x}, {bv:#x}) mismatch");
                let a128 = (u128::from(a ^ 0xdead_beef_cafe_babe) << 64) | u128::from(a);
                assert_eq!(
                    f_half(a128, bv),
                    a128.wrapping_mul(u128::from(bv)),
                    "half_mul({a128:#x}, {bv:#x}) mismatch"
                );
                assert_eq!(
                    f_const(a),
                    u128::from(a).wrapping_mul(0xa076_1d64_78bd_642fu128),
                    "const_mul({a:#x}) mismatch"
                );
            }
        }
    }

    #[test]
    fn multi_pair_ldp_stp_preserves_ascending_offset_order() {
        let alloc = test_alloc();
        let v4f32 = Ty::vector(Scalar::F32, 4).expect("v4f32");

        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I64), Param::of(Ty::F32)],
            &[],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        let loop_hdr = b.create_block(BlockKind::Normal);
        let exit = b.create_block(BlockKind::Normal);

        b.begin(entry);
        let x_ptr = b.param(Ty::PTR);
        let y_ptr = b.param(Ty::PTR);
        let n = b.param(Ty::I64);
        let a_scalar = b.param(Ty::F32);
        let va = b.convert(Op::Splat, Flags::NONE, v4f32, a_scalar);
        let zero = b.iconst(Ty::I64, 0);
        b.jump((loop_hdr, &[x_ptr, y_ptr, zero]));

        b.begin(loop_hdr);
        let px = b.param(Ty::PTR);
        let py = b.param(Ty::PTR);
        let i = b.param(Ty::I64);
        let x0 = b.load(v4f32, Flags::NONE, px, 0, MemAttrs::aligned(4));
        let x1 = b.load(v4f32, Flags::NONE, px, 16, MemAttrs::aligned(4));
        let x2 = b.load(v4f32, Flags::NONE, px, 32, MemAttrs::aligned(4));
        let x3 = b.load(v4f32, Flags::NONE, px, 48, MemAttrs::aligned(4));
        let y0 = b.load(v4f32, Flags::NONE, py, 0, MemAttrs::aligned(4));
        let y1 = b.load(v4f32, Flags::NONE, py, 16, MemAttrs::aligned(4));
        let y2 = b.load(v4f32, Flags::NONE, py, 32, MemAttrs::aligned(4));
        let y3 = b.load(v4f32, Flags::NONE, py, 48, MemAttrs::aligned(4));
        let r0 = b.ternary(Op::Fma, Flags::NONE, va, x0, y0);
        let r1 = b.ternary(Op::Fma, Flags::NONE, va, x1, y1);
        let r2 = b.ternary(Op::Fma, Flags::NONE, va, x2, y2);
        let r3 = b.ternary(Op::Fma, Flags::NONE, va, x3, y3);
        b.store(Flags::NONE, r0, py, 0, MemAttrs::aligned(4));
        b.store(Flags::NONE, r1, py, 16, MemAttrs::aligned(4));
        b.store(Flags::NONE, r2, py, 32, MemAttrs::aligned(4));
        b.store(Flags::NONE, r3, py, 48, MemAttrs::aligned(4));
        let c64 = b.iconst(Ty::I64, 64);
        let c16 = b.iconst(Ty::I64, 16);
        let px_next = b.ptr_add(Flags::NONE, px, c64);
        let py_next = b.ptr_add(Flags::NONE, py, c64);
        let i_next = b.binary(Op::Add, Flags::NONE, i, c16);
        let cond = b.icmp(IntCond::Ult, i_next, n);
        b.br(Flags::NONE, cond, (loop_hdr, &[px_next, py_next, i_next]), (exit, &[]));

        b.begin(exit);
        b.ret(&[]);

        let func = b.finish_checked().expect("4x saxpy LIR builds");
        let emitted = emit_sota_a64(func.view(), &NoNames, CallMode::DirectBl, Machine::LSE, true, alloc)
            .expect("4x saxpy emits");
        let words = decode_words(&emitted.code, alloc);

        let post_q_count = words
            .iter()
            .filter(|&&w| (w & 0xffc0_0000) == 0xacc0_0000 || (w & 0xffc0_0000) == 0xac80_0000)
            .count();
        assert_eq!(
            post_q_count, 0,
            "multi-pair 4x vector loop must not reorder into post-index ldp/stp"
        );

        let mut ldps: Vec<(usize, u32, u32), Alloc<'static>> = Vec::new_in(alloc);
        let mut stps: Vec<(usize, u32, u32), Alloc<'static>> = Vec::new_in(alloc);
        for (pos, &w) in words.iter().enumerate() {
            let rn = (w >> 5) & 0x1f;
            let imm7 = (w >> 15) & 0x7f;
            if (w & 0xffc0_0000) == 0xad40_0000 {
                ldps.push((pos, rn, imm7));
            } else if (w & 0xffc0_0000) == 0xad00_0000 {
                stps.push((pos, rn, imm7));
            }
        }
        assert_eq!(ldps.len(), 4, "expected 4 ldp q (2 for x, 2 for y)");
        assert_eq!(stps.len(), 2, "expected 2 stp q for y");

        // Verify strictly ascending offset order (`[base, #0]` strictly before `[base, #0x20]`) for stores.
        assert_eq!(stps[0].1, stps[1].1, "both stp q must share y base register");
        assert_eq!(stps[0].2, 0, "first stp q must be [base, #0]");
        assert_eq!(stps[1].2, 2, "second stp q must be [base, #0x20]");
        assert!(stps[0].0 < stps[1].0, "stp [base, #0] must precede stp [base, #0x20]");

        // Verify strictly ascending offset order for BOTH load base registers (`x` and `y`).
        let base_a = ldps[0].1;
        let base_b = ldps.iter().find(|x| x.1 != base_a).expect("expected 2 distinct load bases").1;
        for base in [base_a, base_b] {
            let off0 = ldps
                .iter()
                .find(|x| x.1 == base && x.2 == 0)
                .expect("expected ldp [base, #0]")
                .0;
            let off2 = ldps
                .iter()
                .find(|x| x.1 == base && x.2 == 2)
                .expect("expected ldp [base, #0x20]")
                .0;
            assert!(
                off0 < off2,
                "ldp [x{base}, #0] (word {off0}) must precede ldp [x{base}, #0x20] (word {off2})"
            );
        }

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let ex = exec_leaf(emitted);
            let f: extern "C" fn(*const f32, *mut f32, u64, f32) = unsafe { ex.as_fn() };
            let mut x = [0.0f32; 32];
            let mut y = [0.0f32; 32];
            let mut want = [0.0f32; 32];
            let a = 2.5f32;
            for k in 0..32 {
                x[k] = (k as f32) * 0.5 + 1.0;
                y[k] = (k as f32) * -0.25 + 3.0;
                want[k] = a.mul_add(x[k], y[k]);
            }
            f(x.as_ptr(), y.as_mut_ptr(), 32, a);
            for k in 0..32 {
                assert_eq!(y[k].to_bits(), want[k].to_bits(), "saxpy lane {k} mismatch");
            }
        }
    }

    #[test]
    fn wyhash_post_egraph_single_umulh_and_zero_madd() {
        let alloc = test_alloc();

        // 1. Exact post-egraph `wyhash` inner-loop LIR where `((prod >> 64) as u64)` was folded
        //    to `Op::Umulhi(a, b)` and scheduled before `prod = Mul(Zext(a), Zext(b))` whose only
        //    remaining user is `lo = Trunc(prod -> i64)`.
        //    Must fuse `lo` into `Fusion::I128LoMul` -> EXACTLY 1 `umulh`, 1 `mul`, 0 `madd`!
        let wyhash_umulhi_first = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let a = b.param(Ty::I64);
            let bv = b.param(Ty::I64);
            let hi = b.binary(Op::Umulhi, Flags::NONE, a, bv);
            let za = b.convert(Op::Zext, Flags::NONE, Ty::I128, a);
            let zb = b.convert(Op::Zext, Flags::NONE, Ty::I128, bv);
            let prod = b.binary(Op::Mul, Flags::NONE, za, zb);
            let lo = b.convert(Op::Trunc, Flags::NONE, Ty::I64, prod);
            let mixed = b.binary(Op::Xor, Flags::NONE, lo, hi);
            b.ret(&[mixed]);
            let func = b.finish_checked().expect("post-egraph wyhash LIR builds");
            emit_sota_a64(func.view(), &NoNames, CallMode::DirectBl, Machine::LSE, true, alloc)
                .expect("post-egraph wyhash emits")
        };
        let w1 = decode_words(&wyhash_umulhi_first.code, alloc);
        assert_eq!(w1.iter().filter(|&&w| is_umulh(w)).count(), 1, "expected EXACTLY 1 umulh");
        assert_eq!(w1.iter().filter(|&&w| is_mul_x(w)).count(), 1, "expected EXACTLY 1 mul");
        assert_eq!(w1.iter().filter(|&&w| is_madd_x(w)).count(), 0, "expected ZERO madd");

        // 2. General `Trunc(Mul(a: i128, b: i128) -> i64)` (non-zext i128 inputs):
        //    Only low 64 bits are needed -> 0 `umulh`, 1 `mul`, 0 `madd`!
        let lo_mul_i128 = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I128), Param::of(Ty::I128)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let a = b.param(Ty::I128);
            let bv = b.param(Ty::I128);
            let prod = b.binary(Op::Mul, Flags::NONE, a, bv);
            let lo = b.convert(Op::Trunc, Flags::NONE, Ty::I64, prod);
            b.ret(&[lo]);
            let func = b.finish_checked().expect("i128 lo-mul LIR builds");
            emit_sota_a64(func.view(), &NoNames, CallMode::DirectBl, Machine::LSE, true, alloc)
                .expect("i128 lo-mul emits")
        };
        let w2 = decode_words(&lo_mul_i128.code, alloc);
        assert_eq!(w2.iter().filter(|&&w| is_umulh(w)).count(), 0, "lo-only i128 mul must emit 0 umulh");
        assert_eq!(w2.iter().filter(|&&w| is_mul_x(w)).count(), 1, "lo-only i128 mul must emit 1 mul");
        assert_eq!(w2.iter().filter(|&&w| is_madd_x(w)).count(), 0, "lo-only i128 mul must emit 0 madd");

        // 3. Narrow truncation `Trunc(Mul(Zext(a), Zext(b)) -> i32)` -> 0 `umulh`, 1 `mul`, 0 `madd`.
        let lo_mul_i32 = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I32)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let a = b.param(Ty::I64);
            let bv = b.param(Ty::I64);
            let za = b.convert(Op::Zext, Flags::NONE, Ty::I128, a);
            let zb = b.convert(Op::Zext, Flags::NONE, Ty::I128, bv);
            let prod = b.binary(Op::Mul, Flags::NONE, za, zb);
            let lo32 = b.convert(Op::Trunc, Flags::NONE, Ty::I32, prod);
            b.ret(&[lo32]);
            let func = b.finish_checked().expect("i128 lo-mul i32 LIR builds");
            emit_sota_a64(func.view(), &NoNames, CallMode::DirectBl, Machine::LSE, true, alloc)
                .expect("i128 lo-mul i32 emits")
        };
        let w3 = decode_words(&lo_mul_i32.code, alloc);
        assert_eq!(w3.iter().filter(|&&w| is_umulh(w)).count(), 0);
        assert_eq!(w3.iter().filter(|&&w| is_mul_x(w)).count(), 1);
        assert_eq!(w3.iter().filter(|&&w| is_madd_x(w)).count(), 0);

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let ex_wy = exec_leaf(wyhash_umulhi_first);
            let f_wy: extern "C" fn(u64, u64) -> u64 = unsafe { ex_wy.as_fn() };
            let ex_lo = exec_leaf(lo_mul_i128);
            let f_lo: extern "C" fn(u128, u128) -> u64 = unsafe { ex_lo.as_fn() };
            let ex_lo32 = exec_leaf(lo_mul_i32);
            let f_lo32: extern "C" fn(u64, u64) -> u32 = unsafe { ex_lo32.as_fn() };

            let cases: &[(u64, u64)] = &[
                (0, 0),
                (1, 0),
                (1, 1),
                (u64::MAX, u64::MAX),
                (0xa076_1d64_78bd_642f, 0xe703_7ed1_a0b4_28db),
                (0x0123_4567_89ab_cdef, 0xfedc_ba98_7654_3210),
            ];
            for &(a, bv) in cases {
                let p = u128::from(a).wrapping_mul(u128::from(bv));
                let want_wy = (p as u64) ^ ((p >> 64) as u64);
                assert_eq!(f_wy(a, bv), want_wy, "post-egraph wyhash({a:#x}, {bv:#x}) mismatch");
                let a128 = (u128::from(a.rotate_left(17)) << 64) | u128::from(a);
                let b128 = (u128::from(bv.rotate_right(13)) << 64) | u128::from(bv);
                assert_eq!(
                    f_lo(a128, b128),
                    a128.wrapping_mul(b128) as u64,
                    "i128 lo-mul mismatch"
                );
                assert_eq!(
                    f_lo32(a, bv),
                    p as u32,
                    "i128 lo-mul i32 mismatch"
                );
            }
        }
    }

    #[test]
    fn vec_popcnt_neon_all_widths_and_hardware_exec() {
        let alloc = test_alloc();
        let build_popcnt = |elem: Scalar, lanes: u32| -> Emitted<'static> {
            let vty = Ty::vector(elem, lanes).expect("valid 128-bit vector type");
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::PTR), Param::of(Ty::PTR)],
                &[],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let inp = b.param(Ty::PTR);
            let outp = b.param(Ty::PTR);
            let v = b.load(vty, Flags::NONE, inp, 0, MemAttrs::aligned(1));
            let pc = b.unary(Op::Popcnt, Flags::NONE, v);
            b.store(Flags::NONE, pc, outp, 0, MemAttrs::aligned(1));
            b.ret(&[]);
            let func = b.finish_checked().expect("vec popcnt LIR builds");
            emit_sota_a64(func.view(), &NoNames, CallMode::DirectBl, Machine::LSE, true, alloc)
                .expect("vec popcnt emits")
        };

        let is_cnt_16b = |w: u32| (w & 0xffff_fc00) == 0x4e20_5800;
        let is_uaddlp_8h = |w: u32| (w & 0xffff_fc00) == 0x6e20_2800;
        let is_uaddlp_4s = |w: u32| (w & 0xffff_fc00) == 0x6e60_2800;
        let is_uaddlp_2d = |w: u32| (w & 0xffff_fc00) == 0x6ea0_2800;

        let e8 = build_popcnt(Scalar::I8, 16);
        let w8 = decode_words(&e8.code, alloc);
        assert_eq!(w8.iter().filter(|&&w| is_cnt_16b(w)).count(), 1, "v16i8 must emit 1 cnt.16b");
        assert_eq!(w8.iter().filter(|&&w| is_uaddlp_8h(w) || is_uaddlp_4s(w) || is_uaddlp_2d(w)).count(), 0);

        let e16 = build_popcnt(Scalar::I16, 8);
        let w16 = decode_words(&e16.code, alloc);
        assert_eq!(w16.iter().filter(|&&w| is_cnt_16b(w)).count(), 1, "v8i16 must emit 1 cnt.16b");
        assert_eq!(w16.iter().filter(|&&w| is_uaddlp_8h(w)).count(), 1, "v8i16 must emit 1 uaddlp.8h");
        assert_eq!(w16.iter().filter(|&&w| is_uaddlp_4s(w) || is_uaddlp_2d(w)).count(), 0);

        let e32 = build_popcnt(Scalar::I32, 4);
        let w32 = decode_words(&e32.code, alloc);
        assert_eq!(w32.iter().filter(|&&w| is_cnt_16b(w)).count(), 1, "v4i32 must emit 1 cnt.16b");
        assert_eq!(w32.iter().filter(|&&w| is_uaddlp_8h(w)).count(), 1, "v4i32 must emit 1 uaddlp.8h");
        assert_eq!(w32.iter().filter(|&&w| is_uaddlp_4s(w)).count(), 1, "v4i32 must emit 1 uaddlp.4s");
        assert_eq!(w32.iter().filter(|&&w| is_uaddlp_2d(w)).count(), 0);

        let e64 = build_popcnt(Scalar::I64, 2);
        let w64 = decode_words(&e64.code, alloc);
        assert_eq!(w64.iter().filter(|&&w| is_cnt_16b(w)).count(), 1, "v2i64 must emit 1 cnt.16b");
        assert_eq!(w64.iter().filter(|&&w| is_uaddlp_8h(w)).count(), 1, "v2i64 must emit 1 uaddlp.8h");
        assert_eq!(w64.iter().filter(|&&w| is_uaddlp_4s(w)).count(), 1, "v2i64 must emit 1 uaddlp.4s");
        assert_eq!(w64.iter().filter(|&&w| is_uaddlp_2d(w)).count(), 1, "v2i64 must emit 1 uaddlp.2d");

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let ex8 = exec_leaf(e8);
            let f8: extern "C" fn(*const u8, *mut u8) = unsafe { ex8.as_fn() };
            let in8: [u8; 16] = [0x00, 0x01, 0x03, 0x07, 0x0f, 0x1f, 0x3f, 0x7f, 0xff, 0x55, 0xaa, 0x81, 0x42, 0x24, 0x18, 0x99];
            let mut out8 = [0u8; 16];
            f8(in8.as_ptr(), out8.as_mut_ptr());
            for k in 0..16 {
                assert_eq!(out8[k], in8[k].count_ones() as u8, "v16i8 popcnt lane {k}");
            }

            let ex16 = exec_leaf(e16);
            let f16: extern "C" fn(*const u16, *mut u16) = unsafe { ex16.as_fn() };
            let in16: [u16; 8] = [0x0000, 0x0001, 0x8001, 0x5555, 0xaaaa, 0x0fff, 0xfffe, 0xffff];
            let mut out16 = [0u16; 8];
            f16(in16.as_ptr(), out16.as_mut_ptr());
            for k in 0..8 {
                assert_eq!(out16[k], in16[k].count_ones() as u16, "v8i16 popcnt lane {k}");
            }

            let ex32 = exec_leaf(e32);
            let f32_fn: extern "C" fn(*const u32, *mut u32) = unsafe { ex32.as_fn() };
            let in32: [u32; 4] = [0x0000_0000, 0xdead_beef, 0x5555_aaaa, 0xffff_ffff];
            let mut out32 = [0u32; 4];
            f32_fn(in32.as_ptr(), out32.as_mut_ptr());
            for k in 0..4 {
                assert_eq!(out32[k], in32[k].count_ones(), "v4i32 popcnt lane {k}");
            }

            let ex64 = exec_leaf(e64);
            let f64_fn: extern "C" fn(*const u64, *mut u64) = unsafe { ex64.as_fn() };
            let in64: [u64; 2] = [0x0123_4567_89ab_cdef, u64::MAX];
            let mut out64 = [0u64; 2];
            f64_fn(in64.as_ptr(), out64.as_mut_ptr());
            for k in 0..2 {
                assert_eq!(out64[k], u64::from(in64[k].count_ones()), "v2i64 popcnt lane {k}");
            }
        }
    }

    #[test]
    fn vec_extract_neon_fp_and_int_all_lanes_hardware_exec() {
        let alloc = test_alloc();
        let build_extract = |elem: Scalar, lanes: u32, lane: u32| -> Emitted<'static> {
            let vty = Ty::vector(elem, lanes).expect("valid 128-bit vector type");
            let sty = vty.lane();
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR)], &[Param::of(sty)]);
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let p = b.param(Ty::PTR);
            let v = b.load(vty, Flags::NONE, p, 0, MemAttrs::aligned(1));
            let ex = b.extract(v, lane);
            b.ret(&[ex]);
            let func = b.finish_checked().expect("vec extract LIR builds");
            emit_sota_a64(func.view(), &NoNames, CallMode::DirectBl, Machine::LSE, true, alloc)
                .expect("vec extract emits")
        };

        let is_dup_scalar = |w: u32| (w & 0xffe0_fc00) == 0x5e00_0400;
        let is_fmov_reg = |w: u32| (w & 0xffbf_fc00) == 0x1e20_4000;
        let is_umov = |w: u32| (w & 0xbfe0_fc00) == 0x0e00_3c00;

        // 1. `<4 x f32>` across all 4 lanes.
        let f32_vals: [f32; 4] = [1.25, -3.5, 42.75, f32::from_bits(0x3f80_0001)];
        for lane in 0..4u32 {
            let emitted = build_extract(Scalar::F32, 4, lane);
            let words = decode_words(&emitted.code, alloc);
            if lane == 0 {
                assert_eq!(words.iter().filter(|&&w| is_fmov_reg(w) || is_dup_scalar(w)).count(), 0, "f32 lane 0 extract must coalesce to 0 instructions");
            } else {
                assert_eq!(words.iter().filter(|&&w| is_dup_scalar(w)).count(), 1, "f32 lane {lane} extract must emit 1 scalar dup");
            }
            #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
            {
                let ex = exec_leaf(emitted);
                let f: extern "C" fn(*const f32) -> f32 = unsafe { ex.as_fn() };
                assert_eq!(f(f32_vals.as_ptr()).to_bits(), f32_vals[lane as usize].to_bits(), "v4f32 extract lane {lane}");
            }
        }

        // 2. `<2 x f64>` across both lanes.
        let f64_vals: [f64; 2] = [core::f64::consts::PI, -12345.6789];
        for lane in 0..2u32 {
            let emitted = build_extract(Scalar::F64, 2, lane);
            let words = decode_words(&emitted.code, alloc);
            if lane == 0 {
                assert_eq!(words.iter().filter(|&&w| is_fmov_reg(w) || is_dup_scalar(w)).count(), 0, "f64 lane 0 extract must coalesce to 0 instructions");
            } else {
                assert_eq!(words.iter().filter(|&&w| is_dup_scalar(w)).count(), 1, "f64 lane {lane} extract must emit 1 scalar dup");
            }
            #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
            {
                let ex = exec_leaf(emitted);
                let f: extern "C" fn(*const f64) -> f64 = unsafe { ex.as_fn() };
                assert_eq!(f(f64_vals.as_ptr()).to_bits(), f64_vals[lane as usize].to_bits(), "v2f64 extract lane {lane}");
            }
        }

        // 3. `<4 x i32>` across all 4 lanes.
        let i32_vals: [u32; 4] = [0x1122_3344, 0x5566_7788, 0x99aa_bbcc, 0xddee_ff00];
        for lane in 0..4u32 {
            let emitted = build_extract(Scalar::I32, 4, lane);
            let words = decode_words(&emitted.code, alloc);
            assert_eq!(words.iter().filter(|&&w| is_umov(w)).count(), 1, "v4i32 extract lane {lane} must emit 1 umov");
            #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
            {
                let ex = exec_leaf(emitted);
                let f: extern "C" fn(*const u32) -> u32 = unsafe { ex.as_fn() };
                assert_eq!(f(i32_vals.as_ptr()), i32_vals[lane as usize], "v4i32 extract lane {lane}");
            }
        }

        // 4. `<2 x i64>` across both lanes.
        let i64_vals: [u64; 2] = [0x0123_4567_89ab_cdef, 0xfedc_ba98_7654_3210];
        for lane in 0..2u32 {
            let emitted = build_extract(Scalar::I64, 2, lane);
            let words = decode_words(&emitted.code, alloc);
            assert_eq!(words.iter().filter(|&&w| is_umov(w)).count(), 1, "v2i64 extract lane {lane} must emit 1 umov");
            #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
            {
                let ex = exec_leaf(emitted);
                let f: extern "C" fn(*const u64) -> u64 = unsafe { ex.as_fn() };
                assert_eq!(f(i64_vals.as_ptr()), i64_vals[lane as usize], "v2i64 extract lane {lane}");
            }
        }

        // 5. `<16 x i8>` across all 16 lanes.
        let i8_vals: [u8; 16] = [10, 21, 32, 43, 54, 65, 76, 87, 98, 109, 120, 131, 142, 153, 164, 255];
        for lane in 0..16u32 {
            let emitted = build_extract(Scalar::I8, 16, lane);
            let words = decode_words(&emitted.code, alloc);
            assert_eq!(words.iter().filter(|&&w| is_umov(w)).count(), 1, "v16i8 extract lane {lane} must emit 1 umov");
            #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
            {
                let ex = exec_leaf(emitted);
                let f: extern "C" fn(*const u8) -> u8 = unsafe { ex.as_fn() };
                assert_eq!(f(i8_vals.as_ptr()), i8_vals[lane as usize], "v16i8 extract lane {lane}");
            }
        }
    }

    #[test]
    fn vec_fmul_by_element_fuses_splat_zero_dup_hardware_exec() {
        let alloc = test_alloc();
        let v4f32 = Ty::vector(Scalar::F32, 4).expect("v4f32");
        let v2f64 = Ty::vector(Scalar::F64, 2).expect("v2f64");

        let is_dup_elem_v = |w: u32| (w & 0xffe0_fc00) == 0x4e00_0400;
        let is_fmul_4s_elem0 = |w: u32| (w & 0xffe0_fc00) == 0x4f80_9000;
        let is_fmul_2d_elem0 = |w: u32| (w & 0xffe0_fc00) == 0x4fc0_9000;
        let is_fmla_4s_elem0 = |w: u32| (w & 0xffe0_fc00) == 0x4f80_1000;

        // 1. Exact `matmul_f32` strict-IEEE micro-kernel shape:
        //    `a_sc = load f32; va = splat(a_sc); for col in 0..4: c[col] += va * b[col]`.
        let matmul_emitted = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::PTR)],
                &[],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let pa = b.param(Ty::PTR);
            let pb = b.param(Ty::PTR);
            let pc = b.param(Ty::PTR);
            let a_sc = b.load(Ty::F32, Flags::NONE, pa, 0, MemAttrs::aligned(4));
            let va = b.convert(Op::Splat, Flags::NONE, v4f32, a_sc);
            for col in 0..4i32 {
                let off = col * 16;
                let bv = b.load(v4f32, Flags::NONE, pb, off, MemAttrs::aligned(4));
                let cv = b.load(v4f32, Flags::NONE, pc, off, MemAttrs::aligned(4));
                let prod = b.binary(Op::Fmul, Flags::NONE, va, bv);
                let sum = b.binary(Op::Fadd, Flags::NONE, cv, prod);
                b.store(Flags::NONE, sum, pc, off, MemAttrs::aligned(4));
            }
            b.ret(&[]);
            let func = b.finish_checked().expect("matmul 1x16 LIR builds");
            emit_sota_a64(func.view(), &NoNames, CallMode::DirectBl, Machine::LSE, true, alloc)
                .expect("matmul 1x16 emits")
        };
        let mw = decode_words(&matmul_emitted.code, alloc);
        assert_eq!(mw.iter().filter(|&&w| is_dup_elem_v(w)).count(), 0, "matmul_f32 must emit ZERO dup.4s");
        assert_eq!(mw.iter().filter(|&&w| is_fmul_4s_elem0(w)).count(), 4, "matmul_f32 must emit 4 by-element fmul.4s");

        // 2. Entry scalar param in `v0` (`rm < 16`) with `<4 x f32>` FMA + `<2 x f64>` Fmul.
        let param_f32_emitted = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::F32), Param::of(Ty::PTR), Param::of(Ty::PTR)],
                &[],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let a_sc = b.param(Ty::F32);
            let px = b.param(Ty::PTR);
            let py = b.param(Ty::PTR);
            let va = b.convert(Op::Splat, Flags::NONE, v4f32, a_sc);
            let vx = b.load(v4f32, Flags::NONE, px, 0, MemAttrs::aligned(4));
            let vy = b.load(v4f32, Flags::NONE, py, 0, MemAttrs::aligned(4));
            let res = b.ternary(Op::Fma, Flags::NONE, vx, va, vy);
            b.store(Flags::NONE, res, py, 0, MemAttrs::aligned(4));
            b.ret(&[]);
            let func = b.finish_checked().expect("param fma elem LIR builds");
            emit_sota_a64(func.view(), &NoNames, CallMode::DirectBl, Machine::LSE, true, alloc)
                .expect("param fma elem emits")
        };
        let pw = decode_words(&param_f32_emitted.code, alloc);
        assert_eq!(pw.iter().filter(|&&w| is_dup_elem_v(w)).count(), 0, "fma by-element must emit ZERO dup.4s");
        assert_eq!(pw.iter().filter(|&&w| is_fmla_4s_elem0(w)).count(), 1, "expected 1 by-element fmla.4s");

        let f64_emitted = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::F64), Param::of(Ty::PTR), Param::of(Ty::PTR)],
                &[],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let a_sc = b.param(Ty::F64);
            let px = b.param(Ty::PTR);
            let pout = b.param(Ty::PTR);
            let va = b.convert(Op::Splat, Flags::NONE, v2f64, a_sc);
            let vx = b.load(v2f64, Flags::NONE, px, 0, MemAttrs::aligned(8));
            let prod = b.binary(Op::Fmul, Flags::NONE, va, vx);
            b.store(Flags::NONE, prod, pout, 0, MemAttrs::aligned(8));
            b.ret(&[]);
            let func = b.finish_checked().expect("v2f64 fmul elem LIR builds");
            emit_sota_a64(func.view(), &NoNames, CallMode::DirectBl, Machine::LSE, true, alloc)
                .expect("v2f64 fmul elem emits")
        };
        let dw = decode_words(&f64_emitted.code, alloc);
        assert_eq!(dw.iter().filter(|&&w| is_dup_elem_v(w)).count(), 0, "v2f64 fmul by-element must emit ZERO dup.2d");
        assert_eq!(dw.iter().filter(|&&w| is_fmul_2d_elem0(w)).count(), 1, "expected 1 by-element fmul.2d");

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let ex_mm = exec_leaf(matmul_emitted);
            let f_mm: extern "C" fn(*const f32, *const f32, *mut f32) = unsafe { ex_mm.as_fn() };
            let a_val = 2.75f32;
            let b_arr: [f32; 16] = core::array::from_fn(|k| (k as f32) * 0.5 - 3.0);
            let mut c_arr: [f32; 16] = core::array::from_fn(|k| (k as f32) * 1.25 + 0.5);
            let want_c: [f32; 16] = core::array::from_fn(|k| c_arr[k] + a_val * b_arr[k]);
            f_mm(&a_val, b_arr.as_ptr(), c_arr.as_mut_ptr());
            for k in 0..16 {
                assert_eq!(c_arr[k].to_bits(), want_c[k].to_bits(), "matmul by-element lane {k}");
            }

            let ex_fma = exec_leaf(param_f32_emitted);
            let f_fma: extern "C" fn(f32, *const f32, *mut f32) = unsafe { ex_fma.as_fn() };
            let x4 = [1.5f32, -2.25, 3.75, 0.125];
            let mut y4 = [0.5f32, 1.0, -4.0, 2.5];
            let want_y4: [f32; 4] = core::array::from_fn(|k| x4[k].mul_add(-1.75f32, y4[k]));
            f_fma(-1.75f32, x4.as_ptr(), y4.as_mut_ptr());
            for k in 0..4 {
                assert_eq!(y4[k].to_bits(), want_y4[k].to_bits(), "fmla by-element lane {k}");
            }

            let ex_f64 = exec_leaf(f64_emitted);
            let f_f64: extern "C" fn(f64, *const f64, *mut f64) = unsafe { ex_f64.as_fn() };
            let x2 = [3.5f64, -7.25];
            let mut out2 = [0.0f64; 2];
            f_f64(2.5f64, x2.as_ptr(), out2.as_mut_ptr());
            assert_eq!(out2[0].to_bits(), (2.5f64 * x2[0]).to_bits());
            assert_eq!(out2[1].to_bits(), (2.5f64 * x2[1]).to_bits());
        }
    }

    #[test]
    fn rotated_loop_inverted_backedge_zero_trampoline_hardware_exec() {
        let alloc = test_alloc();

        // Build exact `wyhash_stream`-shaped rotated loop with 0 back-edge moves and 1 exit-edge move:
        // - `inverted == true`: `br(i_next == n, (exit, &[acc_next]), (loop_hdr, &[i_next, acc_next]))`
        //   (`!then_nop && else_nop`, hot back-edge is the `else` arm!)
        // - `inverted == false`: `br(i_next != n, (loop_hdr, &[i_next, acc_next]), (exit, &[acc_next]))`
        //   (`then_nop && !else_nop`, hot back-edge is the `then` arm!)
        let build_loop = |inverted: bool| -> Emitted<'static> {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::PTR), Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            let loop_hdr = b.create_block(BlockKind::Normal);
            let exit = b.create_block(BlockKind::Normal);

            b.begin(entry);
            let ptr = b.param(Ty::PTR);
            let n = b.param(Ty::I64);
            let seed = b.param(Ty::I64);
            let zero = b.iconst(Ty::I64, 0);
            let is_empty = b.icmp(IntCond::Eq, n, zero);
            b.br(Flags::NONE, is_empty, (exit, &[seed]), (loop_hdr, &[zero, seed]));

            b.begin(loop_hdr);
            let i = b.param(Ty::I64);
            let acc = b.param(Ty::I64);
            let c3 = b.iconst(Ty::I64, 3);
            let off = b.binary(Op::Shl, Flags::NONE, i, c3);
            let elem_ptr = b.ptr_add(Flags::NONE, ptr, off);
            let val = b.load(Ty::I64, Flags::NONE, elem_ptr, 0, MemAttrs::aligned(8));
            let sum = b.binary(Op::Add, Flags::NONE, acc, val);
            let acc_next = b.binary(Op::Xor, Flags::NONE, sum, i);
            let c1 = b.iconst(Ty::I64, 1);
            let i_next = b.binary(Op::Add, Flags::NONE, i, c1);
            if inverted {
                let done = b.icmp(IntCond::Eq, i_next, n);
                b.br(Flags::NONE, done, (exit, &[acc_next]), (loop_hdr, &[i_next, acc_next]));
            } else {
                let cont = b.icmp(IntCond::Ne, i_next, n);
                b.br(Flags::NONE, cont, (loop_hdr, &[i_next, acc_next]), (exit, &[acc_next]));
            }

            b.begin(exit);
            let res = b.param(Ty::I64);
            b.ret(&[res]);

            let func = b.finish_checked().expect("rotated loop LIR builds");
            emit_sota_a64(func.view(), &NoNames, CallMode::DirectBl, Machine::LSE, true, alloc)
                .expect("rotated loop emits")
        };

        for inverted in [true, false] {
            let emitted = build_loop(inverted);
            let words = decode_words(&emitted.code, alloc);

            let mut back_branch = None;
            for (idx, &w) in words.iter().enumerate() {
                if (w & 0xff00_0010) == 0x5400_0000 {
                    let imm19 = ((w >> 5) & 0x7_ffff) as i32;
                    let disp = (imm19 << 13) >> 13;
                    if disp < 0 {
                        let target = (idx as isize + disp as isize) as usize;
                        back_branch = Some((idx, target));
                    }
                }
            }
            let (b_idx, loop_start) =
                back_branch.expect("expected direct backward b.cond back-edge to loop header");
            assert_ne!(
                words[loop_start] & 0xfc00_0000,
                0x1400_0000,
                "inverted={inverted}: back-edge b.cond must NOT target an unconditional `b` trampoline"
            );
            let uncond_b_in_loop = words[loop_start..=b_idx]
                .iter()
                .filter(|&&w| (w & 0xfc00_0000) == 0x1400_0000)
                .count();
            assert_eq!(
                uncond_b_in_loop, 0,
                "inverted={inverted}: hot loop body must contain ZERO unconditional `b` trampolines"
            );

            #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
            {
                let ex = exec_leaf(emitted);
                let f: extern "C" fn(*const u64, u64, u64) -> u64 = unsafe { ex.as_fn() };
                let data: [u64; 16] = core::array::from_fn(|k| (k as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15).wrapping_add(17));
                for len in [0usize, 1, 2, 7, 16] {
                    let seed = 0x1234_5678_9abc_def0u64;
                    let mut want = seed;
                    for k in 0..len {
                        want = want.wrapping_add(data[k]) ^ (k as u64);
                    }
                    assert_eq!(f(data.as_ptr(), len as u64, seed), want, "inverted={inverted} len={len}");
                }
            }
        }
    }

    #[test]
    fn small_const_memmove_inlines_overlap_safe_and_frameless() {
        let alloc = test_alloc();

        // Every representative length in `0..=32` (exact powers of two, 24, and unaligned odd/tail sizes)
        // must inline with ZERO call instructions, ZERO frame setup, ZERO relocations, and exact
        // overlap-safe semantics for both forward (`dst > src`) and backward (`dst < src`) overlaps.
        for &n in &[0usize, 1, 2, 3, 4, 5, 7, 8, 9, 15, 16, 17, 24, 31, 32] {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(Conv::C, Variadic::No, &[Param::of(Ty::PTR), Param::of(Ty::PTR)], &[]);
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let dst = b.param(Ty::PTR);
            let src = b.param(Ty::PTR);
            let len_c = b.iconst(Ty::I64, n as u128);
            b.bulk(Op::Memmove, Flags::NONE, dst, src, len_c, MemAttrs::aligned(0));
            b.ret(&[]);
            let func = b.finish_checked().expect("small const memmove LIR builds");

            let emitted = emit_sota_a64(func.view(), &NoNames, CallMode::DirectBl, Machine::LSE, true, alloc)
                .expect("small const memmove emits");
            let words = decode_words(&emitted.code, alloc);

            assert!(
                emitted.relocs.is_empty(),
                "n={n}: inline memmove must emit ZERO relocations"
            );
            let has_call = words
                .iter()
                .any(|&w| (w & 0xfc00_0000) == 0x9400_0000 || (w & 0xffff_fc1f) == 0xd63f_0000);
            assert!(!has_call, "n={n}: inline memmove must emit ZERO bl/blr calls");
            assert!(
                words.scan_for(&0xa9bf_7bfd).is_none(),
                "n={n}: pure leaf inline memmove must NOT push fp/lr frame"
            );

            #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
            {
                let ex = exec_leaf(emitted);
                let f: extern "C" fn(*mut u8, *const u8) = unsafe { ex.as_fn() };
                let init: [u8; 128] = core::array::from_fn(|i| ((i.wrapping_mul(37).wrapping_add(13)) & 0xff) as u8);
                let src_off = 32usize;
                // Forward overlaps (`dst = src + k`), identical (`k = 0`), backward overlaps (`dst = src - k`),
                // and disjoint copy (`dst = src + 40`).
                let mut deltas: Vec<isize, Alloc<'static>> = Vec::new_in(alloc);
                deltas.push(0);
                deltas.push(40);
                for k in 1..=n.min(8) {
                    deltas.push(k as isize);
                    deltas.push(-(k as isize));
                }
                for &delta in deltas.iter() {
                    let dst_off = (src_off as isize + delta) as usize;
                    let mut got = init;
                    let mut want = init;
                    want.copy_within(src_off..(src_off + n), dst_off);
                    let base = got.as_mut_ptr();
                    unsafe {
                        f(base.add(dst_off), base.add(src_off));
                    }
                    assert_eq!(got, want, "n={n} delta={delta}: overlap-safe memmove mismatch");
                }
            }
        }

        // Caller-saved values live across an inlined 8-byte `Op::Memmove` (like `slice::swap`)
        // must remain frameless with ZERO calls and preserve live GPR values intact.
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I64), Param::of(Ty::I64), Param::of(Ty::I64)],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);
        let entry = b.create_block(BlockKind::Normal);
        b.begin(entry);
        let dst = b.param(Ty::PTR);
        let src = b.param(Ty::PTR);
        let a = b.param(Ty::I64);
        let bv = b.param(Ty::I64);
        let c = b.param(Ty::I64);
        let ab = b.binary(Op::Add, Flags::NONE, a, bv);
        let t = b.binary(Op::Xor, Flags::NONE, ab, c);
        let eight = b.iconst(Ty::I64, 8);
        b.bulk(Op::Memmove, Flags::NONE, dst, src, eight, MemAttrs::aligned(0));
        let s1 = b.binary(Op::Add, Flags::NONE, t, a);
        let s2 = b.binary(Op::Add, Flags::NONE, s1, bv);
        let res = b.binary(Op::Add, Flags::NONE, s2, c);
        b.ret(&[res]);
        let func = b.finish_checked().expect("live-across memmove LIR builds");

        let emitted = emit_sota_a64(func.view(), &NoNames, CallMode::DirectBl, Machine::LSE, true, alloc)
            .expect("live-across memmove emits");
        let words = decode_words(&emitted.code, alloc);
        let has_call = words
            .iter()
            .any(|&w| (w & 0xfc00_0000) == 0x9400_0000 || (w & 0xffff_fc1f) == 0xd63f_0000);
        assert!(!has_call, "8-byte memmove with live caller-saved GPRs must emit ZERO calls");
        assert!(
            words.scan_for(&0xa9bf_7bfd).is_none(),
            "8-byte memmove with live caller-saved GPRs must stay a frameless leaf"
        );

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let ex = exec_leaf(emitted);
            let f: extern "C" fn(*mut u8, *const u8, u64, u64, u64) -> u64 = unsafe { ex.as_fn() };
            let mut buf = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
            let (av, bv, cv) = (0x1111_2222_3333_4444u64, 0x5555_6666_7777_8888, 0x9999_aaaa_bbbb_cccc);
            let base = buf.as_mut_ptr();
            let got_ret = unsafe { f(base.add(2), base, av, bv, cv) };
            let want_ret = ((av.wrapping_add(bv)) ^ cv).wrapping_add(av).wrapping_add(bv).wrapping_add(cv);
            assert_eq!(got_ret, want_ret);
            assert_eq!(buf, [1, 2, 1, 2, 3, 4, 5, 6, 7, 8, 11, 12]);
        }
    }

    #[test]
    fn multi_use_ptr_add_folds_scaled_load_and_store_cross_block() {
        let alloc = test_alloc();
        let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
        let sig = b.sig(
            Conv::C,
            Variadic::No,
            &[
                Param::of(Ty::PTR),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
                Param::of(Ty::I64),
            ],
            &[Param::of(Ty::I64)],
        );
        b.set_signature(sig);

        let entry = b.create_block(BlockKind::Normal);
        let store_blk = b.create_block(BlockKind::Normal);
        let done_blk = b.create_block(BlockKind::Normal);

        // Block A: `PtrAdd(base, Shl(idx, 3))` used by an offset-0 `I64` `Load`.
        b.begin(entry);
        let base = b.param(Ty::PTR);
        let idx = b.param(Ty::I64);
        let pivot = b.param(Ty::I64);
        let new_val = b.param(Ty::I64);
        let c3 = b.iconst(Ty::I64, 3);
        let off = b.binary(Op::Shl, Flags::NONE, idx, c3);
        let elem_ptr = b.ptr_add(Flags::NONE, base, off);
        let cur = b.load(Ty::I64, Flags::NONE, elem_ptr, 0, MemAttrs::aligned(8));
        let cond = b.icmp(IntCond::Ult, cur, pivot);
        b.br(Flags::NONE, cond, (store_blk, &[]), (done_blk, &[cur]));

        // Block B: SECOND use of the SAME `elem_ptr` in a different block by an offset-0 `I64` `Store`!
        b.begin(store_blk);
        b.store(Flags::NONE, new_val, elem_ptr, 0, MemAttrs::aligned(8));
        b.jump((done_blk, &[cur]));

        b.begin(done_blk);
        let res = b.param(Ty::I64);
        b.ret(&[res]);

        let func = b.finish_checked().expect("cross-block multi-use ptr_add LIR builds");
        let emitted = emit_sota_a64(func.view(), &NoNames, CallMode::DirectBl, Machine::LSE, true, alloc)
            .expect("cross-block multi-use ptr_add emits");
        let words = decode_words(&emitted.code, alloc);

        // 1. ZERO `lsl` (`ubfm` 64-bit: `0xd340_0000` mask `0xffc0_0000`) and ZERO register `add` (`0x8b00_0000` mask `0xff20_0000`)!
        let lsl_count = words
            .iter()
            .filter(|&&w| (w & 0xffc0_0000) == 0xd340_0000)
            .count();
        let add_reg_count = words
            .iter()
            .filter(|&&w| (w & 0xff20_0000) == 0x8b00_0000)
            .count();
        assert_eq!(lsl_count, 0, "multi-use ptr_add must emit ZERO lsl instructions, got {lsl_count}");
        assert_eq!(add_reg_count, 0, "multi-use ptr_add must emit ZERO add instructions, got {add_reg_count}");

        // 2. Exactly 1 scaled 64-bit `ldr xt, [xn, xm, lsl #3]` AND 1 scaled 64-bit `str xt, [xn, xm, lsl #3]`!
        let ldr_scaled_count = words
            .iter()
            .filter(|&&w| (w & 0xffe0_fc00) == 0xf860_7800)
            .count();
        let str_scaled_count = words
            .iter()
            .filter(|&&w| (w & 0xffe0_fc00) == 0xf820_7800)
            .count();
        assert_eq!(ldr_scaled_count, 1, "expected 1 scaled ldr [base, idx, lsl #3], got {ldr_scaled_count}");
        assert_eq!(str_scaled_count, 1, "expected 1 scaled str [base, idx, lsl #3], got {str_scaled_count}");

        // 3. Hardware execution bit-exact on Apple Silicon!
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let ex = exec_leaf(emitted);
            let f: extern "C" fn(*mut u64, u64, u64, u64) -> u64 = unsafe { ex.as_fn() };
            let mut arr = [10u64, 20, 30, 40, 50, 60, 70, 80];

            // Branch taken (`arr[3] == 40 < 55`): returns old 40 and writes 999 into arr[3].
            let r1 = f(arr.as_mut_ptr(), 3, 55, 999);
            assert_eq!(r1, 40);
            assert_eq!(arr, [10, 20, 30, 999, 50, 60, 70, 80]);

            // Branch not taken (`arr[5] == 60 >= 55`): returns 60 and leaves arr untouched.
            let r2 = f(arr.as_mut_ptr(), 5, 55, 777);
            assert_eq!(r2, 60);
            assert_eq!(arr, [10, 20, 30, 999, 50, 60, 70, 80]);
        }
    }

    #[test]
    fn logical_shifted_fuses_lsr_and_into_single_instruction() {
        let alloc = test_alloc();

        let build_case = |ty: Ty, log_op: Op, sh_op: Op, shamt: u64, shift_on_lhs: bool| -> Emitted<'static> {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(ty), Param::of(ty)],
                &[Param::of(ty)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let a = b.param(ty);
            let bv = b.param(ty);
            let k = b.iconst(ty, u128::from(shamt));
            let shifted = b.binary(sh_op, Flags::NONE, bv, k);
            let res = if shift_on_lhs {
                b.binary(log_op, Flags::NONE, shifted, a)
            } else {
                b.binary(log_op, Flags::NONE, a, shifted)
            };
            b.ret(&[res]);
            let func = b.finish_checked().expect("logical shifted LIR builds");
            emit_sota_a64(func.view(), &NoNames, CallMode::DirectBl, Machine::LSE, true, alloc)
                .expect("logical shifted emits")
        };

        // Standalone shift (`ubfm` 32/64-bit `0x5300_0000`/`0xd340_0000` or `sbfm` 32/64-bit `0x1300_0000`/`0x9340_0000`),
        // excluding ABI entry `uxtw xN, wN` (`ubfm xN, xN, #0, #31`, `immr=0, imms=31`, `(w & 0xffff_fc00) == 0xd340_7c00`).
        let is_uxtw = |w: u32| (w & 0xffff_fc00) == 0xd340_7c00;
        let is_standalone_shift = |w: u32| {
            !is_uxtw(w) && ((w & 0x7f80_0000) == 0x5300_0000 || (w & 0x7f80_0000) == 0x1300_0000)
        };
        // Shifted-register `and`/`orr`/`eor` (`N = 0`, `opc != 3`, non-zero `imm6` in bits 15..10).
        let is_shifted_logical = |w: u32| {
            (w & 0x1f20_0000) == 0x0a00_0000 && ((w >> 29) & 3) != 3 && ((w >> 10) & 0x3f) != 0
        };

        // 1. 64-bit cases (including exact `hash_probe_batch` `And(mask, Lshr(x, 44))` in both operand orders).
        let and_lsr64 = build_case(Ty::I64, Op::And, Op::Lshr, 44, false);
        let and_lsr64_lhs = build_case(Ty::I64, Op::And, Op::Lshr, 44, true);
        let or_lsl64 = build_case(Ty::I64, Op::Or, Op::Shl, 8, false);
        let xor_asr64 = build_case(Ty::I64, Op::Xor, Op::Ashr, 3, false);

        // 2. 32-bit cases (`I32`).
        let and_lsr32 = build_case(Ty::I32, Op::And, Op::Lshr, 12, false);
        let or_lsl32 = build_case(Ty::I32, Op::Or, Op::Shl, 8, true);
        let xor_asr32 = build_case(Ty::I32, Op::Xor, Op::Ashr, 3, false);

        for (label, max_words, code) in [
            ("i64 and lsr #44", 3usize, &and_lsr64.code),
            ("i64 and lsr #44 (lhs)", 3, &and_lsr64_lhs.code),
            ("i64 or lsl #8", 3, &or_lsl64.code),
            ("i64 xor asr #3", 3, &xor_asr64.code),
            ("i32 and lsr #12", 4, &and_lsr32.code),
            ("i32 or lsl #8 (lhs)", 4, &or_lsl32.code),
            ("i32 xor asr #3", 4, &xor_asr32.code),
        ] {
            let words = decode_words(code, alloc);
            assert_eq!(
                words.iter().filter(|&&w| is_standalone_shift(w)).count(),
                0,
                "{label}: must emit ZERO standalone shift instructions, words={words:#x?}"
            );
            assert_eq!(
                words.iter().filter(|&&w| is_shifted_logical(w)).count(),
                1,
                "{label}: must emit 1 fused shifted-register logical instruction, words={words:#x?}"
            );
            assert!(
                words.len() <= max_words,
                "{label}: expected minimal instruction count (<= {max_words}), got {} ({words:#x?})",
                words.len()
            );
        }

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let ex_and64 = exec_leaf(and_lsr64);
            let f_and64: extern "C" fn(u64, u64) -> u64 = unsafe { ex_and64.as_fn() };
            let ex_and64_lhs = exec_leaf(and_lsr64_lhs);
            let f_and64_lhs: extern "C" fn(u64, u64) -> u64 = unsafe { ex_and64_lhs.as_fn() };
            let ex_or64 = exec_leaf(or_lsl64);
            let f_or64: extern "C" fn(u64, u64) -> u64 = unsafe { ex_or64.as_fn() };
            let ex_xor64 = exec_leaf(xor_asr64);
            let f_xor64: extern "C" fn(u64, u64) -> u64 = unsafe { ex_xor64.as_fn() };

            let ex_and32 = exec_leaf(and_lsr32);
            let f_and32: extern "C" fn(u32, u32) -> u32 = unsafe { ex_and32.as_fn() };
            let ex_or32 = exec_leaf(or_lsl32);
            let f_or32: extern "C" fn(u32, u32) -> u32 = unsafe { ex_or32.as_fn() };
            let ex_xor32 = exec_leaf(xor_asr32);
            let f_xor32: extern "C" fn(u32, u32) -> u32 = unsafe { ex_xor32.as_fn() };

            let cases64: &[(u64, u64)] = &[
                (0x000f_ffff, 0xfedc_ba98_7654_3210),
                (0xffff_ffff_ffff_ffff, 0x8000_0000_0000_0001),
                (0x1234_5678_9abc_def0, 0x7fff_ffff_ffff_fff8),
                (0x0000_0000_0000_ffff, 0xffff_ffff_ffff_ffff),
            ];
            for &(a, bv) in cases64 {
                assert_eq!(f_and64(a, bv), a & (bv >> 44));
                assert_eq!(f_and64_lhs(a, bv), (bv >> 44) & a);
                assert_eq!(f_or64(a, bv), a | (bv << 8));
                assert_eq!(f_xor64(a, bv), a ^ ((bv as i64) >> 3) as u64);
            }

            let cases32: &[(u32, u32)] = &[
                (0x000f_ffff, 0xfedc_ba98),
                (0xffff_ffff, 0x8000_0001),
                (0x1234_5678, 0x7fff_fff8),
                (0x0000_ffff, 0xffff_ffff),
            ];
            for &(a, bv) in cases32 {
                assert_eq!(f_and32(a, bv), a & (bv >> 12));
                assert_eq!(f_or32(a, bv), (bv << 8) | a);
                assert_eq!(f_xor32(a, bv), a ^ ((bv as i32) >> 3) as u32);
            }
        }
    }

    #[test]
    fn shared_shl_folds_into_dual_array_indexed_loads() {
        let alloc = test_alloc();

        // 1. Same-block `keys[slot]` + `vals[slot]` sharing `off = Shl(slot, 3)` (`use_count[off] == 2`):
        //    must emit ZERO `lsl`, ZERO register `add`, and 2 scaled `ldr xt, [base, slot, lsl #3]`.
        let dual_same_blk = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let keys = b.param(Ty::PTR);
            let vals = b.param(Ty::PTR);
            let slot = b.param(Ty::I64);
            let c3 = b.iconst(Ty::I64, 3);
            let off = b.binary(Op::Shl, Flags::NONE, slot, c3);
            let pk = b.ptr_add(Flags::NONE, keys, off);
            let pv = b.ptr_add(Flags::NONE, vals, off);
            let k = b.load(Ty::I64, Flags::NONE, pk, 0, MemAttrs::aligned(8));
            let v = b.load(Ty::I64, Flags::NONE, pv, 0, MemAttrs::aligned(8));
            let mixed = b.binary(Op::Xor, Flags::NONE, k, v);
            b.ret(&[mixed]);
            let func = b.finish_checked().expect("dual array same-block LIR builds");
            emit_sota_a64(func.view(), &NoNames, CallMode::DirectBl, Machine::LSE, true, alloc)
                .expect("dual array same-block emits")
        };

        // 2. Cross-block `keys[slot]` in `entry` + `vals[slot]` in `hit_blk` (exact `hash_probe_batch` shape!):
        let dual_cross_blk = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::PTR), Param::of(Ty::PTR), Param::of(Ty::I64), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            let hit_blk = b.create_block(BlockKind::Normal);
            let miss_blk = b.create_block(BlockKind::Normal);

            b.begin(entry);
            let keys = b.param(Ty::PTR);
            let vals = b.param(Ty::PTR);
            let slot = b.param(Ty::I64);
            let query = b.param(Ty::I64);
            let c3 = b.iconst(Ty::I64, 3);
            let off = b.binary(Op::Shl, Flags::NONE, slot, c3);
            let pk = b.ptr_add(Flags::NONE, keys, off);
            let k = b.load(Ty::I64, Flags::NONE, pk, 0, MemAttrs::aligned(8));
            let is_hit = b.icmp(IntCond::Eq, k, query);
            b.br(Flags::NONE, is_hit, (hit_blk, &[]), (miss_blk, &[]));

            b.begin(hit_blk);
            let pv = b.ptr_add(Flags::NONE, vals, off);
            let v = b.load(Ty::I64, Flags::NONE, pv, 0, MemAttrs::aligned(8));
            b.ret(&[v]);

            b.begin(miss_blk);
            let zero = b.iconst(Ty::I64, 0);
            b.ret(&[zero]);

            let func = b.finish_checked().expect("dual array cross-block LIR builds");
            emit_sota_a64(func.view(), &NoNames, CallMode::DirectBl, Machine::LSE, true, alloc)
                .expect("dual array cross-block emits")
        };

        // 3. Guard case: `off = Shl(slot, 3)` has a non-`PtrAdd` user (`Xor(k, off)`) -> `off` must remain live!
        let shared_shl_escaping = {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::PTR), Param::of(Ty::I64)],
                &[Param::of(Ty::I64)],
            );
            b.set_signature(sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let keys = b.param(Ty::PTR);
            let slot = b.param(Ty::I64);
            let c3 = b.iconst(Ty::I64, 3);
            let off = b.binary(Op::Shl, Flags::NONE, slot, c3);
            let pk = b.ptr_add(Flags::NONE, keys, off);
            let k = b.load(Ty::I64, Flags::NONE, pk, 0, MemAttrs::aligned(8));
            let res = b.binary(Op::Xor, Flags::NONE, k, off);
            b.ret(&[res]);
            let func = b.finish_checked().expect("escaping shared shl LIR builds");
            emit_sota_a64(func.view(), &NoNames, CallMode::DirectBl, Machine::LSE, true, alloc)
                .expect("escaping shared shl emits")
        };

        for (label, code) in [
            ("same-block dual array", &dual_same_blk.code),
            ("cross-block dual array", &dual_cross_blk.code),
        ] {
            let words = decode_words(code, alloc);
            let lsl_count = words.iter().filter(|&&w| (w & 0xffc0_0000) == 0xd340_0000).count();
            let add_reg_count = words.iter().filter(|&&w| (w & 0xff20_0000) == 0x8b00_0000).count();
            let ldr_scaled_count = words.iter().filter(|&&w| (w & 0xffe0_fc00) == 0xf860_7800).count();
            assert_eq!(lsl_count, 0, "{label}: must emit ZERO lsl instructions, got {lsl_count}");
            assert_eq!(add_reg_count, 0, "{label}: must emit ZERO add instructions, got {add_reg_count}");
            assert_eq!(ldr_scaled_count, 2, "{label}: must emit 2 scaled ldr [base, slot, lsl #3], got {ldr_scaled_count}");
        }
        let esc_words = decode_words(&shared_shl_escaping.code, alloc);
        let esc_lsl_count = esc_words.iter().filter(|&&w| (w & 0xffc0_0000) == 0xd340_0000).count();
        assert_eq!(esc_lsl_count, 1, "escaping shared shl must keep its lsl instruction, got {esc_lsl_count}");

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let keys_arr: [u64; 8] = [101, 202, 303, 404, 505, 606, 707, 808];
            let vals_arr: [u64; 8] = [1111, 2222, 3333, 4444, 5555, 6666, 7777, 8888];

            let ex_same = exec_leaf(dual_same_blk);
            let f_same: extern "C" fn(*const u64, *const u64, u64) -> u64 = unsafe { ex_same.as_fn() };
            for slot in 0..8usize {
                assert_eq!(
                    f_same(keys_arr.as_ptr(), vals_arr.as_ptr(), slot as u64),
                    keys_arr[slot] ^ vals_arr[slot],
                    "same-block dual array mismatch at slot {slot}"
                );
            }

            let ex_cross = exec_leaf(dual_cross_blk);
            let f_cross: extern "C" fn(*const u64, *const u64, u64, u64) -> u64 = unsafe { ex_cross.as_fn() };
            for slot in 0..8usize {
                assert_eq!(
                    f_cross(keys_arr.as_ptr(), vals_arr.as_ptr(), slot as u64, keys_arr[slot]),
                    vals_arr[slot],
                    "cross-block hit mismatch at slot {slot}"
                );
                assert_eq!(
                    f_cross(keys_arr.as_ptr(), vals_arr.as_ptr(), slot as u64, 9999),
                    0,
                    "cross-block miss mismatch at slot {slot}"
                );
            }

            let ex_esc = exec_leaf(shared_shl_escaping);
            let f_esc: extern "C" fn(*const u64, u64) -> u64 = unsafe { ex_esc.as_fn() };
            for slot in 0..8usize {
                assert_eq!(
                    f_esc(keys_arr.as_ptr(), slot as u64),
                    keys_arr[slot] ^ ((slot as u64) << 3),
                    "escaping shl mismatch at slot {slot}"
                );
            }
        }
    }

    #[test]
    fn crc32_target_intrinsics_emit_and_execute_exact() {
        let alloc = test_alloc();
        let emit_crc = |name: &str, data_ty: Ty, features: u64| {
            let mut b = Builder::<Alloc<'static>, Full>::new_in(64, alloc);
            let sig = b.sig(
                Conv::C,
                Variadic::No,
                &[Param::of(Ty::I32), Param::of(data_ty)],
                &[Param::of(Ty::I32)],
            );
            b.set_signature(sig);
            let intr = b.intrinsic(name, sig);
            let entry = b.create_block(BlockKind::Normal);
            b.begin(entry);
            let acc = b.param(Ty::I32);
            let data = b.param(data_ty);
            let out = b.target_intrinsic(intr, &[acc, data]);
            b.ret(&[out]);
            let func = b.finish_checked().expect("crc32 leaf LIR builds");
            emit_sota_a64(func.view(), &NoNames, CallMode::DirectBl, features, true, alloc)
        };

        // Without `Machine::CRC32`, `check_subset` must refuse all `blaze.a64.crc32*` intrinsics.
        assert!(
            emit_crc("blaze.a64.crc32b", Ty::I8, Machine::LSE).is_err(),
            "crc32b must be refused when Machine::CRC32 is absent"
        );
        assert!(
            emit_crc("blaze.a64.crc32cx", Ty::I64, Machine::LSE).is_err(),
            "crc32cx must be refused when Machine::CRC32 is absent"
        );

        let feats = Machine::LSE | Machine::CRC32;
        let crc32b_i8 = emit_crc("blaze.a64.crc32b", Ty::I8, feats).expect("crc32b(i8) emits");
        let crc32b_i32 = emit_crc("blaze.a64.crc32b", Ty::I32, feats).expect("crc32b(i32) emits");
        let crc32x = emit_crc("blaze.a64.crc32x", Ty::I64, feats).expect("crc32x emits");
        let crc32cb_i8 = emit_crc("blaze.a64.crc32cb", Ty::I8, feats).expect("crc32cb(i8) emits");
        let crc32cb_i32 = emit_crc("blaze.a64.crc32cb", Ty::I32, feats).expect("crc32cb(i32) emits");
        let crc32cx = emit_crc("blaze.a64.crc32cx", Ty::I64, feats).expect("crc32cx emits");

        for (label, code, c, sz) in [
            ("crc32b(i8)", &crc32b_i8.code, false, 0u32),
            ("crc32b(i32)", &crc32b_i32.code, false, 0u32),
            ("crc32x", &crc32x.code, false, 3u32),
            ("crc32cb(i8)", &crc32cb_i8.code, true, 0u32),
            ("crc32cb(i32)", &crc32cb_i32.code, true, 0u32),
            ("crc32cx", &crc32cx.code, true, 3u32),
        ] {
            let words = decode_words(code, alloc);
            let want_fixed = crate::enc::crc32(c, sz, 0, 0, 0);
            let crc_count = words
                .iter()
                .filter(|&&w| (w & 0x7fe0_e000) == 0x1ac0_4000 && (w & 0xffe0_fc00) == want_fixed)
                .count();
            assert_eq!(
                crc_count, 1,
                "{label}: expected 1 exact crc32 opcode word ({want_fixed:#010x}), got {crc_count}"
            );
        }

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            const IEEE_POLY: u32 = 0xedb8_8320;
            const CASTAGNOLI_POLY: u32 = 0x82f6_3b78;
            fn crc_model(mut crc: u32, data: u64, bytes: usize, poly: u32) -> u32 {
                for &byte in data.to_le_bytes().iter().take(bytes) {
                    crc ^= u32::from(byte);
                    for _ in 0..8 {
                        crc = (crc >> 1) ^ (poly & (crc & 1).wrapping_neg());
                    }
                }
                crc
            }

            let ex_b_i8 = exec_leaf(crc32b_i8);
            let ex_b_i32 = exec_leaf(crc32b_i32);
            let ex_x = exec_leaf(crc32x);
            let ex_cb_i8 = exec_leaf(crc32cb_i8);
            let ex_cb_i32 = exec_leaf(crc32cb_i32);
            let ex_cx = exec_leaf(crc32cx);

            let f_b_i8: extern "C" fn(u32, u8) -> u32 = unsafe { ex_b_i8.as_fn() };
            let f_b_i32: extern "C" fn(u32, u32) -> u32 = unsafe { ex_b_i32.as_fn() };
            let f_x: extern "C" fn(u32, u64) -> u32 = unsafe { ex_x.as_fn() };
            let f_cb_i8: extern "C" fn(u32, u8) -> u32 = unsafe { ex_cb_i8.as_fn() };
            let f_cb_i32: extern "C" fn(u32, u32) -> u32 = unsafe { ex_cb_i32.as_fn() };
            let f_cx: extern "C" fn(u32, u64) -> u32 = unsafe { ex_cx.as_fn() };

            for &(acc, byte) in &[
                (0xffff_ffffu32, 0x61u8),
                (0, 0),
                (0x1234_5678, 0xff),
                (0xdead_beef, 0x80),
                (0xcafe_babe, 0x39),
            ] {
                let want_ieee = crc_model(acc, u64::from(byte), 1, IEEE_POLY);
                let want_c = crc_model(acc, u64::from(byte), 1, CASTAGNOLI_POLY);
                assert_eq!(f_b_i8(acc, byte), want_ieee, "crc32b(i8) {acc:#x}, {byte:#x}");
                assert_eq!(
                    f_b_i32(acc, u32::from(byte) | 0xdead_be00),
                    want_ieee,
                    "crc32b(i32) {acc:#x}, {byte:#x}"
                );
                assert_eq!(f_cb_i8(acc, byte), want_c, "crc32cb(i8) {acc:#x}, {byte:#x}");
                assert_eq!(
                    f_cb_i32(acc, u32::from(byte) | 0xfeed_fa00),
                    want_c,
                    "crc32cb(i32) {acc:#x}, {byte:#x}"
                );
            }

            for &(acc, word) in &[
                (0xffff_ffffu32, 0x0123_4567_89ab_cdefu64),
                (0, 0),
                (1, u64::MAX),
                (0xcafe_babe, 0x8000_0000_0000_0001),
                (0xdead_beef, u64::from_le_bytes(*b"12345678")),
            ] {
                assert_eq!(
                    f_x(acc, word),
                    crc_model(acc, word, 8, IEEE_POLY),
                    "crc32x {acc:#x}, {word:#x}"
                );
                assert_eq!(
                    f_cx(acc, word),
                    crc_model(acc, word, 8, CASTAGNOLI_POLY),
                    "crc32cx {acc:#x}, {word:#x}"
                );
            }

            // Standard check values on `"123456789"`:
            // IEEE CRC-32 = `0xcbf4_3926`, Castagnoli CRC-32C = `0xe306_9283`.
            let mut ieee_by_byte = 0xffff_ffffu32;
            let mut crc32c_by_byte = 0xffff_ffffu32;
            for &byte in b"123456789" {
                ieee_by_byte = f_b_i8(ieee_by_byte, byte);
                crc32c_by_byte = f_cb_i8(crc32c_by_byte, byte);
            }
            assert_eq!(!ieee_by_byte, 0xcbf4_3926, "IEEE CRC-32(\"123456789\") byte-by-byte");
            assert_eq!(!crc32c_by_byte, 0xe306_9283, "Castagnoli CRC-32C(\"123456789\") byte-by-byte");

            let w8 = u64::from_le_bytes(*b"12345678");
            assert_eq!(
                !f_b_i8(f_x(0xffff_ffff, w8), b'9'),
                0xcbf4_3926,
                "IEEE CRC-32(\"123456789\") via crc32x + crc32b"
            );
            assert_eq!(
                !f_cb_i8(f_cx(0xffff_ffff, w8), b'9'),
                0xe306_9283,
                "Castagnoli CRC-32C(\"123456789\") via crc32cx + crc32cb"
            );
        }
    }
}
