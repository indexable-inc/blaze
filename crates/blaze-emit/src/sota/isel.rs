//! ISLE-Style Bottom-Up Pattern-Fusing AArch64 Emitter for `TierName::Sota`.
//!
//! Emits frameless (for leaf functions) native AArch64 machine code directly from verified,
//! `aegraph` + LICM + strength-reduced `blaze-lir`:
//!
//! - **Fused Multiply-Add / Multiply-Sub**: `add(mul(a, b), c)` -> `madd`, `sub(c, mul(a, b))` -> `msub`
//! - **Fused Shifted-Register ALU**: `add(a, shl(b, k))` -> `add lsl #k`, `add(a, lsr(b, k))` -> `add lsr #k`, `sub(a, shl(b, k))` -> `sub lsl #k`
//! - **Zero-Materialization Immediates**: `add_imm`, `sub_imm`, `cmp_imm`, `lsl_imm`, `lsr_imm`, `sbfm` (`asr #k`), and `ubfm` (`ubfx`) for power-of-2 bitmasks (`x & (2^k - 1)`)
//! - **Fused Compare + Branch / Select**: `icmp + br` -> `cmp`/`cmp_imm` + `b.cond` (or `cbz`/`cbnz` against 0); `icmp + select` -> `cmp`/`cmp_imm` + `csel` (zero `cset`!)
//! - **Cycle-Safe Parallel-Copy Resolver**: optimal block-param & call-arg register shuffling with `X16` cycle breaker + fallthrough branch elimination
//! - **Frameless Leaf Functions**: zero `stp`/`ldp`/`sp` traffic when no calls or callee-saved registers are needed.

use blaze_code::record::{Space, Symbol, Target};
use blaze_lir::{Block, Flags, IntCond, Op, Ty, View};
use blaze_own::{Alloc, Vec};
use blaze_tier::{Emitted, Named, Names, Outside, Reloc, RelocKind, TierError, TierName};

use super::regalloc::{self, ARG_REGS, NO_REG};
use crate::enc::{self, Cond, FP, LR, Reg, SP, Size, X0, X16, ZR};
use crate::pass::{BRK_DIV_OVERFLOW, BRK_DIV_ZERO, BRK_UNREACHABLE};

const X17: Reg = 17;
const TIER: TierName = TierName::Sota;

/// Fused instruction selection pattern chosen for an instruction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fusion {
    None,
    /// `add(mul(a, b), c)` -> `madd rd, a, b, c`.
    Madd { a: u32, b: u32, acc: u32 },
    /// `sub(c, mul(a, b))` -> `msub rd, a, b, c`.
    Msub { a: u32, b: u32, acc: u32 },
    /// `add(a, shl(b, k))` -> `add rd, a, b, lsl #k`.
    AddLsl { a: u32, b: u32, shift: u32 },
    /// `add(a, lshr(b, k))` -> `add rd, a, b, lsr #k`.
    AddLsr { a: u32, b: u32, shift: u32 },
    /// `sub(a, shl(b, k))` -> `sub rd, a, b, lsl #k`.
    SubLsl { a: u32, b: u32, shift: u32 },
    /// `br(icmp(cond, a, b), then, else)` -> fused `cmp`/`cbz` + `b.cond`.
    CmpBr { cond: IntCond, a: u32, b: u32, op_ty: Ty },
    /// `select(icmp(cond, a, b), t, f)` -> fused `cmp` + `csel`.
    CmpSelect { cond: IntCond, a: u32, b: u32, op_ty: Ty },
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

#[inline]
fn is_supported_scalar(ty: Ty) -> bool {
    if ty == Ty::NONE {
        return true;
    }
    ty.is_ptr() || (ty.is_scalar() && ty.is_int() && ty.bits(64) <= 64)
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
fn is_low_bitmask(v: u64) -> Option<u32> {
    // True iff `v == (1 << k) - 1` for `k` in `1..=63` -> single `ubfm rd, rn, #0, #(k-1)`.
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

/// Emits a 64-bit constant into `rd` using minimal `movz`/`movn`/`movk` instructions.
fn emit_const_u64(words: &mut Vec<u32, Alloc<'_>>, rd: Reg, val: u64) {
    if val == 0 {
        words.push(enc::movz(rd, 0, 0));
        return;
    }
    // Check if `!val` fits in one 16-bit chunk (`movn`).
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

/// Narrow `rd` to `bits` zero-extended when `bits < 64`.
#[inline]
fn emit_narrow(words: &mut Vec<u32, Alloc<'_>>, rd: Reg, bits: u32) {
    if bits < 64 {
        words.push(enc::zext(rd, rd, bits));
    }
}

/// Resolves an arbitrary simultaneous parallel register copy `[(dst, src)]` using `X16` as cycle
/// breaker (`X16` is never allocated to any SSA value).
fn emit_parallel_moves(words: &mut Vec<u32, Alloc<'_>>, moves: &mut Vec<(Reg, Reg), Alloc<'_>>) {
    // Strip no-op self moves.
    let mut i = 0;
    while i < moves.len() {
        if moves[i].0 == moves[i].1 {
            moves.remove_swap(i);
        } else {
            i += 1;
        }
    }

    while !moves.is_empty() {
        // Find a move whose `dst` is not read as `src` by any other pending move.
        let mut ready = None;
        for idx in 0..moves.len() {
            let dst = moves[idx].0;
            let clobbers_src = moves.iter().any(|&(_, s)| s == dst);
            if !clobbers_src {
                ready = Some(idx);
                break;
            }
        }
        if let Some(idx) = ready {
            let (dst, src) = moves.remove_swap(idx);
            words.push(enc::mov(dst, src));
        } else {
            // Pure cycle: save `moves[0].0` in `X16`, redirect all reads of `moves[0].0` to `X16`.
            let (dst, src) = moves.remove_swap(0);
            words.push(enc::mov(X16, dst));
            for m in moves.iter_mut() {
                if m.1 == dst {
                    m.1 = X16;
                }
            }
            words.push(enc::mov(dst, src));
        }
    }
}

/// Validates that `view` is inside `Sota`'s direct AArch64 fused subset, returning `Err` naming the
/// exact offending instruction so `Sota::with_fallback` can cleanly delegate exotic ops.
fn check_subset(view: &View<'_>) -> Result<(), TierError> {
    if view.blocks() == 0 {
        return Err(TierError::Refused { tier: TIER, inst: None, what: "empty function" });
    }
    if !view.data_bytes.is_empty() {
        return Err(TierError::Refused { tier: TIER, inst: None, what: "own data" });
    }
    // Check function signature: up to 8 scalar integer/ptr params, at most 1 scalar return.
    let entry_params = view.params(Block(0));
    if entry_params.len() > ARG_REGS.len() {
        return Err(TierError::Refused { tier: TIER, inst: None, what: "more than 8 parameters" });
    }
    for inst in 0..view.len() {
        let ty = Ty(view.tys[inst]);
        if !is_supported_scalar(ty) {
            return Err(TierError::Refused { tier: TIER, inst: Some(inst as u32), what: "non-scalar-int64 type" });
        }
        let op = view.op(inst);
        let ok = matches!(
            op,
            Op::Param
                | Op::Iconst
                | Op::IconstWide
                | Op::Null
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
                | Op::Select
                | Op::Zext
                | Op::Sext
                | Op::Trunc
                | Op::Freeze
                | Op::Jump
                | Op::Br
                | Op::Return
                | Op::Trap
                | Op::Unreachable
                | Op::Call
        );
        if !ok {
            return Err(TierError::Refused { tier: TIER, inst: Some(inst as u32), what: op.name() });
        }
        if op == Op::Call {
            let at = view.data[inst] as usize;
            let argc = view.extra[at + 2] as usize;
            if argc > ARG_REGS.len() {
                return Err(TierError::Refused { tier: TIER, inst: Some(inst as u32), what: "call with >8 args" });
            }
        }
        if op == Op::Return {
            let at = view.data[inst] as usize;
            let retc = view.extra[at] as usize;
            if retc > 1 {
                return Err(TierError::Refused { tier: TIER, inst: Some(inst as u32), what: "multi-value return" });
            }
        }
    }
    Ok(())
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

    // 1. Precompute block ownership, use counts, and inline constants.
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

    // 2. ISLE-style single-use same-block pattern fusion detection!
    let mut fusion: Vec<Fusion, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    let mut skip_emit: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    for _ in 0..len {
        fusion.push(Fusion::None);
        skip_emit.push(false);
    }

    // Mark constants that are exclusively consumed as immediate operands (never materialize!).
    let mut imm_uses: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(len, alloc);
    for _ in 0..len {
        imm_uses.push(0);
    }

    for inst in 0..len {
        let op = view.op(inst);
        let ty = Ty(view.tys[inst]);
        let bits = if ty == Ty::NONE { 0 } else { ty.bits(64) };
        let b = inst_block[inst];
        let flag_free = Flags(view.flags[inst]).0 == 0;

        match op {
            Op::Add if flag_free && bits == 64 => {
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
                if let Some((sop, a, sb, shift, sk)) = try_shift(rhs, lhs).or_else(|| try_shift(lhs, rhs)) {
                    let sh_inst = if try_shift(rhs, lhs).is_some() { rhs } else { lhs };
                    fusion[inst] = if sop == Op::Shl {
                        Fusion::AddLsl { a, b: sb, shift }
                    } else {
                        Fusion::AddLsr { a, b: sb, shift }
                    };
                    skip_emit[sh_inst as usize] = true;
                    imm_uses[sk as usize] += 1;
                    continue;
                }
                // Immediate add_imm check (`0..=4095`).
                if const_u64[rhs as usize].is_some_and(|v| v <= 4095) {
                    imm_uses[rhs as usize] += 1;
                } else if const_u64[lhs as usize].is_some_and(|v| v <= 4095) {
                    imm_uses[lhs as usize] += 1;
                }
            }
            Op::Sub if flag_free && bits == 64 => {
                let at = view.data[inst] as usize;
                let lhs = view.extra[at];
                let rhs = view.extra[at + 1];
                let ri = rhs as usize;
                // sub(c, mul(a, b)) -> msub
                if use_count[ri] == 1 && inst_block[ri] == b && view.op(ri) == Op::Mul && !skip_emit[ri] {
                    let pat = view.data[ri] as usize;
                    fusion[inst] = Fusion::Msub { a: view.extra[pat], b: view.extra[pat + 1], acc: lhs };
                    skip_emit[ri] = true;
                    continue;
                }
                // sub(a, shl(b, k)) -> sub_lsl
                if use_count[ri] == 1 && inst_block[ri] == b && view.op(ri) == Op::Shl && !skip_emit[ri] {
                    let sat = view.data[ri] as usize;
                    let sb = view.extra[sat];
                    let sk = view.extra[sat + 1];
                    if let Some(k) = const_u64[sk as usize] {
                        fusion[inst] = Fusion::SubLsl { a: lhs, b: sb, shift: (k & 63) as u32 };
                        skip_emit[ri] = true;
                        imm_uses[sk as usize] += 1;
                        continue;
                    }
                }
                if const_u64[ri].is_some_and(|v| v <= 4095) {
                    imm_uses[ri] += 1;
                }
            }
            Op::Add | Op::Sub => {
                let at = view.data[inst] as usize;
                let rhs = view.extra[at + 1] as usize;
                if const_u64[rhs].is_some_and(|v| v <= 4095) {
                    imm_uses[rhs] += 1;
                }
            }
            Op::And if flag_free => {
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
            Op::Br => {
                let at = view.data[inst] as usize;
                let cond_v = view.extra[at] as usize;
                if use_count[cond_v] == 1 && inst_block[cond_v] == b && view.op(cond_v) == Op::Icmp && !skip_emit[cond_v] {
                    let cat = view.data[cond_v] as usize;
                    if let Some(cond) = IntCond::from_word(view.extra[cat]) {
                        let a = view.extra[cat + 1];
                        let bv = view.extra[cat + 2];
                        let op_ty = Ty(view.tys[a as usize]);
                        fusion[inst] = Fusion::CmpBr { cond, a, b: bv, op_ty };
                        skip_emit[cond_v] = true;
                        if op_ty.bits(64) == 64 && const_u64[bv as usize].is_some_and(|v| v <= 4095) {
                            imm_uses[bv as usize] += 1;
                        }
                    }
                }
            }
            Op::Select => {
                let at = view.data[inst] as usize;
                let cond_v = view.extra[at] as usize;
                if use_count[cond_v] == 1 && inst_block[cond_v] == b && view.op(cond_v) == Op::Icmp && !skip_emit[cond_v] {
                    let cat = view.data[cond_v] as usize;
                    if let Some(cond) = IntCond::from_word(view.extra[cat]) {
                        let a = view.extra[cat + 1];
                        let bv = view.extra[cat + 2];
                        let op_ty = Ty(view.tys[a as usize]);
                        fusion[inst] = Fusion::CmpSelect { cond, a, b: bv, op_ty };
                        skip_emit[cond_v] = true;
                        if op_ty.bits(64) == 64 && const_u64[bv as usize].is_some_and(|v| v <= 4095) {
                            imm_uses[bv as usize] += 1;
                        }
                    }
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
        }
    }

    // Any constant whose every use is an immediate position needs ZERO register & ZERO emission!
    for inst in 0..len {
        if const_u64[inst].is_some() && use_count[inst] > 0 && imm_uses[inst] == use_count[inst] {
            skip_emit[inst] = true;
        }
    }

    // 3. Chordal-SSA register allocation (skipping fused producers and pure-immediate constants!).
    let reg_alloc = regalloc::allocate(&view, &skip_emit, alloc).map_err(|_| TierError::Refused {
        tier: TIER,
        inst: None,
        what: "register pressure exceeded Sota GPR budget",
    })?;
    let reg_of = |v: u32| -> Reg {
        let r = reg_alloc.reg_of[v as usize];
        debug_assert!(r != NO_REG, "use of unallocated value {v}");
        u32::from(r)
    };

    // 4. Emit AArch64 words!
    let mut words: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(len * 2 + 16, alloc);
    let mut patches: Vec<BranchPatch, Alloc<'a>> = Vec::new_in(alloc);
    let mut block_word_off: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(blocks, alloc);
    for _ in 0..blocks {
        block_word_off.push(0);
    }
    let mut moves: Vec<(Reg, Reg), Alloc<'a>> = Vec::with_capacity_in(16, alloc);
    let mut out = Emitted::new_in(alloc);

    // Frameless leaf prologue when no calls and no callee-saved regs!
    let needs_frame = reg_alloc.has_calls || !reg_alloc.callee_saved.is_empty();
    let cs_pairs = reg_alloc.callee_saved.len().div_ceil(2);
    if needs_frame {
        words.push(enc::stp_pre(FP, LR, SP, -16));
        words.push(enc::add_imm(FP, SP, 0, false)); // mov x29, sp
        for p in 0..cs_pairs {
            let r1 = reg_alloc.callee_saved[p * 2];
            let r2 = reg_alloc.callee_saved.get(p * 2 + 1).copied().unwrap_or(ZR);
            words.push(enc::stp_pre(r1, r2, SP, -16));
        }
        out.frame = (cs_pairs * 16) as u32;
    }

    // Move entry parameters from `X0..X7` to their allocated registers (usually zero moves due to coalescing!).
    let entry_params = view.params(Block(0));
    moves.clear();
    for (k, p) in entry_params.clone().enumerate() {
        let dst = reg_alloc.reg_of[p];
        if dst != NO_REG {
            moves.push((u32::from(dst), ARG_REGS[k]));
        }
    }
    emit_parallel_moves(&mut words, &mut moves);
    // Narrow sub-64-bit entry params to keep zero-extension invariant.
    for p in entry_params {
        let dst = reg_alloc.reg_of[p];
        if dst != NO_REG {
            let bits = Ty(view.tys[p]).bits(64);
            emit_narrow(&mut words, u32::from(dst), bits);
        }
    }

    // Helper: emit signed/unsigned compare of `a` and `b` setting NZCV flags.
    let emit_cmp = |words: &mut Vec<u32, Alloc<'a>>, cond: IntCond, a: u32, bv: u32, op_ty: Ty| {
        let bits = op_ty.bits(64);
        let ra = reg_of(a);
        let signed = matches!(cond, IntCond::Slt | IntCond::Sle | IntCond::Sgt | IntCond::Sge);
        if bits == 64 && const_u64[bv as usize].is_some_and(|v| v <= 4095) {
            let imm = const_u64[bv as usize].unwrap() as u32;
            words.push(enc::cmp_imm(ra, imm));
        } else if signed && bits < 64 {
            let rb = reg_of(bv);
            words.push(enc::sext(X16, ra, bits));
            words.push(enc::sext(X17, rb, bits));
            words.push(enc::cmp(X16, X17));
        } else {
            let rb = reg_of(bv);
            words.push(enc::cmp(ra, rb));
        }
    };

    // Helper: emit parallel moves for a branch/jump target's block arguments.
    let emit_edge_moves = |words: &mut Vec<u32, Alloc<'a>>,
                           moves: &mut Vec<(Reg, Reg), Alloc<'a>>,
                           target_block: u32,
                           args_start: usize,
                           args_len: usize| {
        let target_params = view.params(Block(target_block));
        moves.clear();
        for k in 0..args_len {
            let param_inst = target_params.start + k;
            let dst = reg_alloc.reg_of[param_inst];
            if dst == NO_REG {
                continue;
            }
            let src_v = view.extra[args_start + k];
            if let Some(cval) = const_u64[src_v as usize] {
                if skip_emit[src_v as usize] {
                    // Edge arg was a skipped immediate constant -> materialize into X17 or dst later.
                    emit_const_u64(words, u32::from(dst), cval);
                    continue;
                }
            }
            moves.push((u32::from(dst), reg_of(src_v)));
        }
        emit_parallel_moves(words, moves);
    };

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
            let bits = if ty == Ty::NONE { 0 } else { ty.bits(64) };
            let rd_raw = reg_alloc.reg_of[inst];

            // Skip dead non-side-effecting instructions.
            if !op.is_terminator() && op != Op::Call && op != Op::Trap && rd_raw == NO_REG {
                continue;
            }
            let rd = u32::from(rd_raw);

            match op {
                Op::Param => {}
                Op::Null => words.push(enc::movz(rd, 0, 0)),
                Op::Iconst | Op::IconstWide => {
                    let val = const_u64[inst].unwrap_or(0);
                    emit_const_u64(&mut words, rd, val);
                }
                Op::Freeze => {
                    let src = reg_of(view.data[inst]);
                    if rd != src {
                        words.push(enc::mov(rd, src));
                    }
                }
                Op::Neg => {
                    let src = reg_of(view.data[inst]);
                    words.push(enc::neg(rd, src));
                    emit_narrow(&mut words, rd, bits);
                }
                Op::Not => {
                    let src = reg_of(view.data[inst]);
                    words.push(enc::mvn(rd, src));
                    emit_narrow(&mut words, rd, bits);
                }
                Op::Add => match fusion[inst] {
                    Fusion::Madd { a, b: mb, acc } => {
                        words.push(enc::madd(rd, reg_of(a), reg_of(mb), reg_of(acc)));
                    }
                    Fusion::AddLsl { a, b: sb, shift } => {
                        words.push(enc::add_lsl(rd, reg_of(a), reg_of(sb), shift));
                    }
                    Fusion::AddLsr { a, b: sb, shift } => {
                        words.push(enc::add_lsr(rd, reg_of(a), reg_of(sb), shift));
                    }
                    _ => {
                        let at = view.data[inst] as usize;
                        let lhs = view.extra[at];
                        let rhs = view.extra[at + 1];
                        if let Some(imm) = const_u64[rhs as usize].filter(|&v| v <= 4095) {
                            words.push(enc::add_imm(rd, reg_of(lhs), imm as u32, false));
                        } else if let Some(imm) = const_u64[lhs as usize].filter(|&v| v <= 4095) {
                            words.push(enc::add_imm(rd, reg_of(rhs), imm as u32, false));
                        } else {
                            words.push(enc::add(rd, reg_of(lhs), reg_of(rhs)));
                        }
                        emit_narrow(&mut words, rd, bits);
                    }
                },
                Op::Sub => match fusion[inst] {
                    Fusion::Msub { a, b: mb, acc } => {
                        words.push(enc::msub(rd, reg_of(a), reg_of(mb), reg_of(acc)));
                    }
                    Fusion::SubLsl { a, b: sb, shift } => {
                        words.push(enc::sub_lsl(rd, reg_of(a), reg_of(sb), shift));
                    }
                    _ => {
                        let at = view.data[inst] as usize;
                        let lhs = view.extra[at];
                        let rhs = view.extra[at + 1];
                        if let Some(imm) = const_u64[rhs as usize].filter(|&v| v <= 4095) {
                            words.push(enc::sub_imm(rd, reg_of(lhs), imm as u32, false));
                        } else {
                            words.push(enc::sub(rd, reg_of(lhs), reg_of(rhs)));
                        }
                        emit_narrow(&mut words, rd, bits);
                    }
                },
                Op::Mul => {
                    let at = view.data[inst] as usize;
                    let lhs = view.extra[at];
                    let rhs = view.extra[at + 1];
                    words.push(enc::mul(rd, reg_of(lhs), reg_of(rhs)));
                    emit_narrow(&mut words, rd, bits);
                }
                Op::And => {
                    let at = view.data[inst] as usize;
                    let lhs = view.extra[at];
                    let rhs = view.extra[at + 1];
                    if let Some(k) = const_u64[rhs as usize].and_then(is_low_bitmask) {
                        words.push(enc::ubfm(rd, reg_of(lhs), 0, k - 1));
                    } else if let Some(k) = const_u64[lhs as usize].and_then(is_low_bitmask) {
                        words.push(enc::ubfm(rd, reg_of(rhs), 0, k - 1));
                    } else {
                        words.push(enc::and(rd, reg_of(lhs), reg_of(rhs)));
                    }
                }
                Op::Or => {
                    let at = view.data[inst] as usize;
                    words.push(enc::orr(rd, reg_of(view.extra[at]), reg_of(view.extra[at + 1])));
                }
                Op::Xor => {
                    let at = view.data[inst] as usize;
                    words.push(enc::eor(rd, reg_of(view.extra[at]), reg_of(view.extra[at + 1])));
                }
                Op::Shl => {
                    let at = view.data[inst] as usize;
                    let lhs = view.extra[at];
                    let rhs = view.extra[at + 1];
                    if bits == 64 {
                        if let Some(k) = const_u64[rhs as usize] {
                            let sh = (k & 63) as u32;
                            if sh == 0 {
                                if rd != reg_of(lhs) {
                                    words.push(enc::mov(rd, reg_of(lhs)));
                                }
                            } else {
                                words.push(enc::lsl_imm(rd, reg_of(lhs), sh));
                            }
                        } else {
                            words.push(enc::lslv(rd, reg_of(lhs), reg_of(rhs)));
                        }
                    } else {
                        let mask_bits = bits.trailing_zeros();
                        if mask_bits == 0 {
                            words.push(enc::mov(rd, reg_of(lhs)));
                        } else {
                            words.push(enc::ubfm(X17, reg_of(rhs), 0, mask_bits - 1));
                            words.push(enc::lslv(rd, reg_of(lhs), X17));
                            emit_narrow(&mut words, rd, bits);
                        }
                    }
                }
                Op::Lshr => {
                    let at = view.data[inst] as usize;
                    let lhs = view.extra[at];
                    let rhs = view.extra[at + 1];
                    if bits == 64 {
                        if let Some(k) = const_u64[rhs as usize] {
                            let sh = (k & 63) as u32;
                            if sh == 0 {
                                if rd != reg_of(lhs) {
                                    words.push(enc::mov(rd, reg_of(lhs)));
                                }
                            } else {
                                words.push(enc::lsr_imm(rd, reg_of(lhs), sh));
                            }
                        } else {
                            words.push(enc::lsrv(rd, reg_of(lhs), reg_of(rhs)));
                        }
                    } else {
                        let mask_bits = bits.trailing_zeros();
                        if mask_bits == 0 {
                            words.push(enc::mov(rd, reg_of(lhs)));
                        } else {
                            words.push(enc::ubfm(X17, reg_of(rhs), 0, mask_bits - 1));
                            words.push(enc::lsrv(rd, reg_of(lhs), X17));
                            emit_narrow(&mut words, rd, bits);
                        }
                    }
                }
                Op::Ashr => {
                    let at = view.data[inst] as usize;
                    let lhs = view.extra[at];
                    let rhs = view.extra[at + 1];
                    if bits == 64 {
                        if let Some(k) = const_u64[rhs as usize] {
                            let sh = (k & 63) as u32;
                            if sh == 0 {
                                if rd != reg_of(lhs) {
                                    words.push(enc::mov(rd, reg_of(lhs)));
                                }
                            } else {
                                words.push(enc::sbfm(rd, reg_of(lhs), sh, 63));
                            }
                        } else {
                            words.push(enc::asrv(rd, reg_of(lhs), reg_of(rhs)));
                        }
                    } else {
                        let mask_bits = bits.trailing_zeros();
                        if mask_bits == 0 {
                            words.push(enc::mov(rd, reg_of(lhs)));
                        } else {
                            words.push(enc::sext(X16, reg_of(lhs), bits));
                            words.push(enc::ubfm(X17, reg_of(rhs), 0, mask_bits - 1));
                            words.push(enc::asrv(rd, X16, X17));
                            emit_narrow(&mut words, rd, bits);
                        }
                    }
                }
                Op::Rotr => {
                    let at = view.data[inst] as usize;
                    let lhs = view.extra[at];
                    let rhs = view.extra[at + 1];
                    if bits == 64 {
                        words.push(enc::rorv(rd, reg_of(lhs), reg_of(rhs)));
                    } else {
                        return Err(TierError::Refused { tier: TIER, inst: Some(inst as u32), what: "sub-64 rotr" });
                    }
                }
                Op::Udiv => {
                    let at = view.data[inst] as usize;
                    let ra = reg_of(view.extra[at]);
                    let rb = reg_of(view.extra[at + 1]);
                    words.push(enc::cbnz(rb, 2));
                    words.push(enc::brk(BRK_DIV_ZERO));
                    words.push(enc::udiv(rd, ra, rb));
                    emit_narrow(&mut words, rd, bits);
                }
                Op::Urem => {
                    let at = view.data[inst] as usize;
                    let ra = reg_of(view.extra[at]);
                    let rb = reg_of(view.extra[at + 1]);
                    words.push(enc::cbnz(rb, 2));
                    words.push(enc::brk(BRK_DIV_ZERO));
                    words.push(enc::udiv(X16, ra, rb));
                    words.push(enc::msub(rd, X16, rb, ra));
                    emit_narrow(&mut words, rd, bits);
                }
                Op::Sdiv => {
                    let at = view.data[inst] as usize;
                    let ra = reg_of(view.extra[at]);
                    let rb = reg_of(view.extra[at + 1]);
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
                    let after_ov = words.len();
                    words[skip_ov] = enc::b_cond(Cond::Ne, (after_ov - skip_ov) as i32);
                    // Reload b_reg if tmp clobbered X17.
                    let b_final = if bits < 64 {
                        words.push(enc::sext(X17, rb, bits));
                        X17
                    } else {
                        rb
                    };
                    let a_final = if bits < 64 {
                        words.push(enc::sext(X16, ra, bits));
                        X16
                    } else {
                        ra
                    };
                    words.push(enc::sdiv(rd, a_final, b_final));
                    emit_narrow(&mut words, rd, bits);
                }
                Op::Srem => {
                    let at = view.data[inst] as usize;
                    let ra = reg_of(view.extra[at]);
                    let rb = reg_of(view.extra[at + 1]);
                    words.push(enc::cbnz(rb, 2));
                    words.push(enc::brk(BRK_DIV_ZERO));
                    let (a_reg, b_reg) = if bits < 64 {
                        words.push(enc::sext(X16, ra, bits));
                        words.push(enc::sext(X17, rb, bits));
                        (X16, X17)
                    } else {
                        (ra, rb)
                    };
                    words.push(enc::sdiv(X16, a_reg, b_reg));
                    let a_again = if bits < 64 {
                        words.push(enc::sext(X17, ra, bits));
                        X17
                    } else {
                        ra
                    };
                    // Recompute b_reg cleanly if needed; simplest when `bits == 64` (0 overhead).
                    if bits < 64 {
                        words.push(enc::sdiv(rd, a_again, rb)); // already handled above; use X16
                        return Err(TierError::Refused { tier: TIER, inst: Some(inst as u32), what: "sub-64 srem" });
                    }
                    words.push(enc::msub(rd, X16, rb, ra));
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
                    emit_cmp(&mut words, cond, a, bv, Ty(view.tys[a as usize]));
                    words.push(enc::cset(rd, cond_of(cond)));
                }
                Op::Select => {
                    let at = view.data[inst] as usize;
                    let t = view.extra[at + 1];
                    let f = view.extra[at + 2];
                    let rt = reg_of(t);
                    let rf = reg_of(f);
                    match fusion[inst] {
                        Fusion::CmpSelect { cond, a, b: bv, op_ty } => {
                            emit_cmp(&mut words, cond, a, bv, op_ty);
                            words.push(enc::csel(rd, rt, rf, cond_of(cond)));
                        }
                        _ => {
                            let rc = reg_of(view.extra[at]);
                            words.push(enc::cmp_imm(rc, 0));
                            words.push(enc::csel(rd, rt, rf, Cond::Ne));
                        }
                    }
                }
                Op::Zext => {
                    let src = reg_of(view.data[inst]);
                    if rd != src {
                        words.push(enc::mov(rd, src));
                    }
                    emit_narrow(&mut words, rd, bits);
                }
                Op::Sext => {
                    let src_v = view.data[inst];
                    let src_bits = Ty(view.tys[src_v as usize]).bits(64);
                    let src = reg_of(src_v);
                    if src_bits < 64 {
                        words.push(enc::sext(rd, src, src_bits));
                    } else if rd != src {
                        words.push(enc::mov(rd, src));
                    }
                    emit_narrow(&mut words, rd, bits);
                }
                Op::Trunc => {
                    let src = reg_of(view.data[inst]);
                    if bits < 64 {
                        words.push(enc::zext(rd, src, bits));
                    } else if rd != src {
                        words.push(enc::mov(rd, src));
                    }
                }
                Op::Call => {
                    let at = view.data[inst] as usize;
                    let ref_id = view.extra[at];
                    let argc = view.extra[at + 2] as usize;
                    let target = match names.name(Outside::Func(ref_id)) {
                        Some(Named::Symbol(symbol)) => {
                            let idx = out.targets.len() as u32;
                            out.targets.push(Target { space: Space::Func, symbol });
                            idx
                        }
                        Some(Named::C(name)) => {
                            let sym = Symbol::of_name(name);
                            let start = out.text.len() as u32;
                            out.text.extend_from_slice(name.as_bytes());
                            out.names.push((sym, start, name.len() as u32));
                            let idx = out.targets.len() as u32;
                            out.targets.push(Target { space: Space::Outside, symbol: sym });
                            idx
                        }
                        None => {
                            return Err(TierError::NoName { tier: TIER, of: Outside::Func(ref_id) });
                        }
                    };
                    // Marshal call arguments into X0..X7 via parallel move.
                    moves.clear();
                    for k in 0..argc {
                        let arg_v = view.extra[at + 3 + k];
                        moves.push((ARG_REGS[k], reg_of(arg_v)));
                    }
                    emit_parallel_moves(&mut words, &mut moves);

                    let adrp_at = (words.len() * 4) as u32;
                    words.push(enc::adrp(X16, 0));
                    words.push(enc::ldr(Size::X, X16, X16, 0));
                    out.relocs.push(Reloc { at: adrp_at, in_relro: false, kind: RelocKind::GotPage21, target, addend: 0 });
                    out.relocs.push(Reloc {
                        at: adrp_at + 4,
                        in_relro: false,
                        kind: RelocKind::GotPageOff12,
                        target,
                        addend: 0,
                    });
                    words.push(enc::blr(X16));
                    if rd_raw != NO_REG && ty != Ty::NONE {
                        if rd != X0 {
                            words.push(enc::mov(rd, X0));
                        }
                        emit_narrow(&mut words, rd, bits);
                    }
                }
                Op::Jump => {
                    let at = view.data[inst] as usize;
                    let target_block = view.extra[at];
                    let argc = view.extra[at + 1] as usize;
                    emit_edge_moves(&mut words, &mut moves, target_block, at + 2, argc);
                    // Fallthrough elimination: if jumping to immediately next block, omit `b`!
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
                        // Fast zero-edge-arg fused compare-and-branch!
                        match fusion[inst] {
                            Fusion::CmpBr { cond, a, b: bv, op_ty } => {
                                let is_zero_cmp = const_u64[bv as usize] == Some(0)
                                    && matches!(cond, IntCond::Eq | IntCond::Ne);
                                if is_zero_cmp {
                                    let ra = reg_of(a);
                                    if else_block as usize == b + 1 {
                                        let kind = if cond == IntCond::Eq {
                                            PatchKind::Cbz(ra)
                                        } else {
                                            PatchKind::Cbnz(ra)
                                        };
                                        let widx = words.len();
                                        words.push(enc::cbz(ra, 0));
                                        patches.push(BranchPatch { word_idx: widx, kind, target_block: then_block });
                                    } else if then_block as usize == b + 1 {
                                        let kind = if cond == IntCond::Eq {
                                            PatchKind::Cbnz(ra)
                                        } else {
                                            PatchKind::Cbz(ra)
                                        };
                                        let widx = words.len();
                                        words.push(enc::cbnz(ra, 0));
                                        patches.push(BranchPatch { word_idx: widx, kind, target_block: else_block });
                                    } else {
                                        let kind = if cond == IntCond::Eq {
                                            PatchKind::Cbz(ra)
                                        } else {
                                            PatchKind::Cbnz(ra)
                                        };
                                        let w1 = words.len();
                                        words.push(enc::cbz(ra, 0));
                                        patches.push(BranchPatch { word_idx: w1, kind, target_block: then_block });
                                        let w2 = words.len();
                                        words.push(enc::b(0));
                                        patches.push(BranchPatch {
                                            word_idx: w2,
                                            kind: PatchKind::B,
                                            target_block: else_block,
                                        });
                                    }
                                } else {
                                    emit_cmp(&mut words, cond, a, bv, op_ty);
                                    let c = cond_of(cond);
                                    if else_block as usize == b + 1 {
                                        let widx = words.len();
                                        words.push(enc::b_cond(c, 0));
                                        patches.push(BranchPatch {
                                            word_idx: widx,
                                            kind: PatchKind::BCond(c),
                                            target_block: then_block,
                                        });
                                    } else if then_block as usize == b + 1 {
                                        let inv = match c.invert() {
                                            0 => Cond::Eq,
                                            1 => Cond::Ne,
                                            2 => Cond::Hs,
                                            3 => Cond::Lo,
                                            8 => Cond::Hi,
                                            9 => Cond::Ls,
                                            10 => Cond::Ge,
                                            11 => Cond::Lt,
                                            12 => Cond::Gt,
                                            _ => Cond::Le,
                                        };
                                        let widx = words.len();
                                        words.push(enc::b_cond(inv, 0));
                                        patches.push(BranchPatch {
                                            word_idx: widx,
                                            kind: PatchKind::BCond(inv),
                                            target_block: else_block,
                                        });
                                    } else {
                                        let w1 = words.len();
                                        words.push(enc::b_cond(c, 0));
                                        patches.push(BranchPatch {
                                            word_idx: w1,
                                            kind: PatchKind::BCond(c),
                                            target_block: then_block,
                                        });
                                        let w2 = words.len();
                                        words.push(enc::b(0));
                                        patches.push(BranchPatch {
                                            word_idx: w2,
                                            kind: PatchKind::B,
                                            target_block: else_block,
                                        });
                                    }
                                }
                            }
                            _ => {
                                let rc = reg_of(cond_v);
                                if else_block as usize == b + 1 {
                                    let widx = words.len();
                                    words.push(enc::cbnz(rc, 0));
                                    patches.push(BranchPatch {
                                        word_idx: widx,
                                        kind: PatchKind::Cbnz(rc),
                                        target_block: then_block,
                                    });
                                } else {
                                    let w1 = words.len();
                                    words.push(enc::cbnz(rc, 0));
                                    patches.push(BranchPatch {
                                        word_idx: w1,
                                        kind: PatchKind::Cbnz(rc),
                                        target_block: then_block,
                                    });
                                    let w2 = words.len();
                                    words.push(enc::b(0));
                                    patches.push(BranchPatch {
                                        word_idx: w2,
                                        kind: PatchKind::B,
                                        target_block: else_block,
                                    });
                                }
                            }
                        }
                    } else {
                        // Branch with block arguments: test cond, jump over `then` edge-moves if false.
                        let skip_then_idx = match fusion[inst] {
                            Fusion::CmpBr { cond, a, b: bv, op_ty } => {
                                emit_cmp(&mut words, cond, a, bv, op_ty);
                                let inv = match cond_of(cond).invert() {
                                    0 => Cond::Eq,
                                    1 => Cond::Ne,
                                    2 => Cond::Hs,
                                    3 => Cond::Lo,
                                    8 => Cond::Hi,
                                    9 => Cond::Ls,
                                    10 => Cond::Ge,
                                    11 => Cond::Lt,
                                    12 => Cond::Gt,
                                    _ => Cond::Le,
                                };
                                let idx = words.len();
                                words.push(enc::b_cond(inv, 0));
                                (idx, Some(inv))
                            }
                            _ => {
                                let rc = reg_of(cond_v);
                                let idx = words.len();
                                words.push(enc::cbz(rc, 0));
                                (idx, None)
                            }
                        };
                        emit_edge_moves(&mut words, &mut moves, then_block, at + 3, then_argc);
                        let w_then = words.len();
                        words.push(enc::b(0));
                        patches.push(BranchPatch { word_idx: w_then, kind: PatchKind::B, target_block: then_block });

                        let else_start = words.len();
                        let delta = (else_start - skip_then_idx.0) as i32;
                        words[skip_then_idx.0] = match skip_then_idx.1 {
                            Some(inv) => enc::b_cond(inv, delta),
                            None => enc::cbz(reg_of(cond_v), delta),
                        };
                        emit_edge_moves(&mut words, &mut moves, else_block, else_at + 2, else_argc);
                        if else_block as usize != b + 1 {
                            let w_else = words.len();
                            words.push(enc::b(0));
                            patches.push(BranchPatch {
                                word_idx: w_else,
                                kind: PatchKind::B,
                                target_block: else_block,
                            });
                        }
                    }
                }
                Op::Return => {
                    let at = view.data[inst] as usize;
                    let retc = view.extra[at] as usize;
                    if retc == 1 {
                        let rv = view.extra[at + 1];
                        if let Some(cval) = const_u64[rv as usize] {
                            if skip_emit[rv as usize] {
                                emit_const_u64(&mut words, X0, cval);
                            } else {
                                let src = reg_of(rv);
                                if src != X0 {
                                    words.push(enc::mov(X0, src));
                                }
                            }
                        } else {
                            let src = reg_of(rv);
                            if src != X0 {
                                words.push(enc::mov(X0, src));
                            }
                        }
                    }
                    if needs_frame {
                        for p in (0..cs_pairs).rev() {
                            let r1 = reg_alloc.callee_saved[p * 2];
                            let r2 = reg_alloc.callee_saved.get(p * 2 + 1).copied().unwrap_or(ZR);
                            words.push(enc::ldp_post(r1, r2, SP, 16));
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
