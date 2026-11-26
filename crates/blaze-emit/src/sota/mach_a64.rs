#![cfg_attr(not(test), forbid(unsafe_code))]
//! Post-RA AArch64 Machine IR & Optimizer (`mach_a64`).
//!
//! Sits directly between `isel.rs` instruction emission and final `BranchPatch` resolution /
//! code sealing. Operates on a Structure-of-Arrays (SoA) decoded instruction table backed by
//! the caller's explicit [`Alloc`], preserving 100% soundness by classifying any unrecognized
//! instruction word as a full conservative barrier (`InstInfo::conservative_barrier`).
//!
//! # Passes
//! 1. **Pass 1 — `mach_peephole_a64`** (with exact CFG-level cross-block `u32` GPR liveness):
//!    - Self-move (`mov xd, xd`, 64-bit only) & dead pure move elimination
//!    - `cset` + `cmp #0` + `csel` flag-forwarding & hazard-checked hoisting across intervening flag writers
//!    - `sub rd, rn, #1` + `cinc rd, rd, cc` (+ `cmp rd, #0`) commutation into `cinc rd, rn, cc` +
//!      `sub[s] rd, rd, #1` (eliminating `cmp rd, #0` when downstream flag consumers test only `Z`: `eq`/`ne`)
//!    - Flag-setting fusion (`add/sub rd, ...` + `cmp rd, #0` -> `adds/subs rd, ...`) and
//!      redundant `cmp rd, #0` elimination after `adds`/`subs`/`ands`/`bics` when downstream
//!      flag consumers only observe `N`/`Z`: `eq`/`ne`/`mi`/`pl`
//!    - Redundant straight-line store-to-load reload elimination (`str` + `ldr` same slot -> delete or `mov`)
//!    - General live-range anti-dependency `csel` rename & reorder (eliminates loop-latch and
//!      rotated loop-header-entry `mov x_dst, x_tmp` across any `(x_dst, x_tmp, r_scratch)` triple)
//!    - Forward & backward copy propagation
//!    - `lsr` + `ubfx` bitfield extraction fusion (`peephole_fuse_lsr_ubfx`)
//!    - Dead narrow `ubfx #0, #{8,16}` elision before byte/halfword stores, `orr ..., lsl #{24,16}`,
//!      and narrow `ubfm` consumers (`peephole_elide_dead_narrow_ubfx`)
//! 1b. **Pass 1b — `mach_redundant_loads`**:
//!    - Intra-MBB straight-line redundant load elimination & store-to-load forwarding
//!      (stack-allocated 16-entry fact table, exact same-base non-overlapping store aliasing,
//!      `W32` zero-extending `mov wt, wval` forwarding, and duplicate base-pointer spill reload
//!      elimination immediately before Pass 2 to unlock `LDP`/`STP` pairing).
//! 2. **Pass 2 — `mach_pair_ldp_stp`**:
//!    - Cross-ALU non-aliasing `LDP`/`STP` pairing for GPR (`W32`/`X64`) and SIMD&FP
//!      (`S32`/`D64`/`Q128`) within a 12-instruction straight-line window.
//! 2b. **Pass 2b — `mach_fuse_post_index`**:
//!    - Fuses zero-offset `LDP`/`STP`/`LDR`/`STR` (`Q128`/`X64`/`D64`) with a subsequent
//!      same-base `add`/`sub rn, rn, #imm` into single post-index writeback instructions
//!      when `rn` is not `SP`, `rt != rn` for GPRs, and no intervening instruction touches `rn`.
//! 3. **Pass 3 — `mach_sched_m5`**:
//!    - Post-RA Apple M5 / Graviton 5 critical-path list scheduler (6 ALU / 3 LDST / 4 FP pipes,
//!      4-cycle load latency, 3-cycle multiply latency) with exact RAW/WAR/WAW + memory DAG
//!      and macro-op fusion lock keeping `cmp`/`adds`/`subs`/`tst` immediately before `b.cc`.
//! 4. **Pass 4 — `mach_branch_opt` + Exact Offset Remapping**:
//!    - Tiny trampoline tail-duplication (+ post-dup `movz #imm16; lsr #sh` folding), fallthrough
//!      unconditional `b` elision + O(N) compaction remapping `block_word_off`, `patch_word_idxs`,
//!      `reloc_byte_offs`, `trap_byte_offs`, and pre-resolved internal branch displacements in lockstep.

use blaze_own::{Alloc, Vec};

use crate::enc::{self, Field};

/// Sentinel written into `patch_word_idxs[p]` when an unconditional fallthrough branch
/// patch is elided by Pass 4 (`mach_branch_opt`).
pub const ELIDED_PATCH: usize = usize::MAX;

/// Maximum fixed-point iterations for backward CFG GPR liveness (bounded progress, Rule 18).
const MAX_LIVENESS_ITERS: usize = 64;

/// Maximum instruction distance `j - i` considered by Pass 2 (`mach_pair_ldp_stp`).
const PAIR_WINDOW: usize = 12;

/// Forward lookahead bound `j < i + POST_INDEX_WINDOW` for Pass 2b (`mach_fuse_post_index`).
const POST_INDEX_WINDOW: usize = 6;

/// Minimum straight-line region length scheduled by Pass 3 (`mach_sched_m5`).
const SCHED_MIN_LEN: usize = 4;

/// Maximum straight-line region length scheduled by Pass 3 (`mach_sched_m5`).
const SCHED_MAX_LEN: usize = 128;

/// Maximum non-NOP instructions in a predecessor block `P` eligible for trampoline tail-duplication.
const TAIL_DUP_MAX_PRED_INSTS: usize = 4;

/// Maximum non-NOP instructions in a trampoline block `T` eligible for tail-duplication.
const TAIL_DUP_MAX_TRAMP_INSTS: usize = 6;

/// Maximum total instructions duplicated per function by trampoline tail-duplication.
const TAIL_DUP_MAX_TOTAL_INSTS: usize = 64;

/// Sentinel MBB index meaning "no successor / no predecessor".
const NO_MBB: u32 = u32::MAX;

// Per-instruction `flags_rw` bit layout.
const FLAG_READS_NZCV: u8 = 1 << 0;
const FLAG_WRITES_NZCV: u8 = 1 << 1;
const FLAG_BRANCH_OR_BARRIER: u8 = 1 << 2;

// Per-instruction `state` bit layout.
const STATE_DELETED: u8 = 1 << 0;
const STATE_PINNED: u8 = 1 << 1;
const STATE_RELOC_OR_TRAP: u8 = 1 << 2;
const STATE_TARGET_BOUNDARY: u8 = 1 << 3;
const STATE_OPAQUE_DATA: u8 = 1 << 4;

/// Callee-saved GPRs (`X19..=X30`) and `SP` (bit 31) that are always live-in at `ret`.
pub const RET_CALLEE_SAVED_GPRS: u32 = 0xfff8_0000;

/// Conservative default GPR bitmask used by `ret`: `X0..=X7` (all AAPCS64 return regs),
/// `X19..=X30` (callee-saved + FP/LR), and `SP`.
/// `X8..=X18` are caller-saved scratch registers (including `sret` input `X8`) and are dead on function exit.
pub const RET_LIVE_GPRS: u32 = 0xfff8_00ff;

/// Scratch GPR candidates that may always be reused if dead (caller-saved `X0..=X15`, excluding `X16..=X18`).
const CALLER_SCRATCH_GPRS: u32 = 0x0000_ffff;

/// Callee-saved GPRs (`X19..=X28`) that may be reused as `r_scratch` ONLY if already written by the function.
const CALLEE_SAVED_GPRS: u32 = 0x1ff8_0000;

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
fn i64_of(n: usize) -> Option<i64> {
    i64::try_from(n).ok()
}

#[inline]
const fn reg5(val: u32) -> u8 {
    (val & 31).to_le_bytes()[0]
}

#[inline]
const fn u4_low(val: u32) -> u8 {
    (val & 15).to_le_bytes()[0]
}

#[inline]
const fn u12_low(val: u32) -> u16 {
    let b = (val & 0xfff).to_le_bytes();
    u16::from_le_bytes([b[0], b[1]])
}

#[inline]
const fn sext_bits(val: u32, bits: u32) -> i32 {
    let shift = 32 - bits;
    (val << shift).cast_signed() >> shift
}

/// Bitmask for a GPR operand where encoding 31 means `XZR`/`WZR` (zero register -> bitmask 0).
#[inline]
const fn gpr_bit_zr(reg: u8) -> u32 {
    if reg < 31 { 1u32 << reg } else { 0 }
}

/// Bitmask for a GPR operand where encoding 31 means `SP` (stack pointer -> bit 31).
#[inline]
const fn gpr_bit_sp(reg: u8) -> u32 {
    1u32 << (reg & 31)
}

/// Bitmask for a SIMD/FP register `V0..=V31`.
#[inline]
const fn fp_bit(reg: u8) -> u32 {
    1u32 << (reg & 31)
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
fn at_usize(s: &[usize], i: usize) -> usize {
    s.get(i).copied().unwrap_or(0)
}

#[inline]
fn set_at<T>(s: &mut [T], i: usize, v: T) {
    if let Some(slot) = s.get_mut(i) {
        *slot = v;
    }
}

fn vec_filled_in<'a, T: Copy>(len: usize, val: T, alloc: Alloc<'a>) -> Vec<T, Alloc<'a>> {
    let mut v = Vec::with_capacity_in(len, alloc);
    for _ in 0..len {
        v.push(val);
    }
    v
}

// =============================================================================
// 1. Compact Decoded AArch64 Machine Instruction Classification (`InstInfo`)
// =============================================================================

/// Memory effect of an AArch64 instruction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MemEffect {
    None,
    Load,
    Store,
    Barrier,
}

/// Width of a scalar or vector memory access.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MemSize {
    B8,
    H16,
    W32,
    X64,
    Q128,
}

impl MemSize {
    /// Byte width of one element of this access size.
    #[inline]
    pub const fn bytes(self) -> i32 {
        match self {
            Self::B8 => 1,
            Self::H16 => 2,
            Self::W32 => 4,
            Self::X64 => 8,
            Self::Q128 => 16,
        }
    }
}

/// Apple M5 / Graviton 5 execution pipe classification.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum M5Pipe {
    Alu,
    LdSt,
    Fp,
    Branch,
}

impl M5Pipe {
    #[inline]
    const fn index(self) -> usize {
        match self {
            Self::Alu => 0,
            Self::LdSt => 1,
            Self::Fp => 2,
            Self::Branch => 3,
        }
    }
}

/// Structured fast-match classification for post-RA AArch64 optimizations.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DecodedKind {
    /// `orr xd, xzr, xm` (`wide == true`: pure 64-bit move) or `orr wd, wzr, wm` (`wide == false`: 32-bit zero-extending move).
    MovReg { rd: u8, rn: u8, wide: bool },
    /// `csel xd, xn, xm, cond` / `csel wd, wn, wm, cond`.
    Csel { rd: u8, rn: u8, rm: u8, cond: u8, wide: bool },
    /// `cset xd, cond` / `cset wd, cond` (`csinc rd, xzr, xzr, invert(cond)` with `cond < 14`).
    Cset { rd: u8, cond: u8, wide: bool },
    /// `ldr`/`ldur` with signed byte offset `imm_bytes` from base `rn`.
    LdrOff { rt: u8, rn: u8, imm_bytes: i32, size: MemSize, is_fp: bool },
    /// `str`/`stur` with signed byte offset `imm_bytes` from base `rn`.
    StrOff { rt: u8, rn: u8, imm_bytes: i32, size: MemSize, is_fp: bool },
    /// `subs xzr, xn, #0` / `subs wzr, wn, #0`.
    CmpZero { rn: u8, wide: bool },
    /// `add`/`sub`/`adds`/`subs` immediate (excluding `CmpZero`).
    AddSubImm { rd: u8, rn: u8, imm12: u16, shift12: bool, is_sub: bool, wide: bool, sets_flags: bool },
    /// Unshifted `add`/`sub`/`adds`/`subs` register.
    AddSubReg { rd: u8, rn: u8, rm: u8, is_sub: bool, wide: bool, sets_flags: bool },
    /// Unconditional PC-relative branch `b #delta_words`.
    BranchUncond { delta_words: i32 },
    /// Conditional PC-relative branch (`b.cc`, `cbz`, `cbnz`, `tbz`, `tbnz`).
    BranchCond { delta_words: i32, is_flag_cond: bool, cond: u8 },
    /// Direct or indirect call (`bl`, `blr`).
    Call,
    /// Function return (`ret`).
    Ret,
    /// Indirect branch (`br xn`).
    IndirectBranch,
    /// Architectural `nop`.
    Nop,
    /// Any other decoded or opaque instruction.
    Other,
}

/// Complete sound architectural & microarchitectural metadata for one 32-bit AArch64 word.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct InstInfo {
    /// Bitmask of GPRs `0..=30` and `SP` (bit 31) written. `XZR` never sets bit 31.
    pub gpr_defs: u32,
    /// Bitmask of GPRs `0..=30` and `SP` (bit 31) read. `XZR` never sets bit 31.
    pub gpr_uses: u32,
    /// Bitmask of SIMD/FP registers `V0..=V31` written.
    pub fp_defs: u32,
    /// Bitmask of SIMD/FP registers `V0..=V31` read.
    pub fp_uses: u32,
    /// True iff instruction reads NZCV condition flags.
    pub reads_flags: bool,
    /// True iff instruction modifies NZCV condition flags.
    pub writes_flags: bool,
    /// Memory side effect classification.
    pub mem: MemEffect,
    /// True for branches, calls, `ret`, `brk`, `adr`/`adrp`, relocations, or unknown opcodes.
    pub is_branch_or_barrier: bool,
    /// Apple M5 execution latency in cycles.
    pub latency: u8,
    /// Apple M5 execution pipe.
    pub pipe: M5Pipe,
    /// Fast-match instruction kind.
    pub kind: DecodedKind,
}

impl InstInfo {
    /// Full conservative barrier returned for any unrecognized, atomic, system, or exotic opcode.
    ///
    /// Guarantees ZERO unsound reordering, deletion, or rewriting of unknown instructions.
    #[inline]
    pub const fn conservative_barrier() -> Self {
        Self {
            gpr_defs: u32::MAX,
            gpr_uses: u32::MAX,
            fp_defs: u32::MAX,
            fp_uses: u32::MAX,
            reads_flags: true,
            writes_flags: true,
            mem: MemEffect::Barrier,
            is_branch_or_barrier: true,
            latency: 4,
            pipe: M5Pipe::Alu,
            kind: DecodedKind::Other,
        }
    }

    #[inline]
    const fn alu(
        gpr_defs: u32,
        gpr_uses: u32,
        reads_flags: bool,
        writes_flags: bool,
        latency: u8,
        kind: DecodedKind,
    ) -> Self {
        Self {
            gpr_defs,
            gpr_uses,
            fp_defs: 0,
            fp_uses: 0,
            reads_flags,
            writes_flags,
            mem: MemEffect::None,
            is_branch_or_barrier: false,
            latency,
            pipe: M5Pipe::Alu,
            kind,
        }
    }

    #[inline]
    const fn flags_byte(&self) -> u8 {
        let mut b = 0u8;
        if self.reads_flags {
            b |= FLAG_READS_NZCV;
        }
        if self.writes_flags {
            b |= FLAG_WRITES_NZCV;
        }
        if self.is_branch_or_barrier {
            b |= FLAG_BRANCH_OR_BARRIER;
        }
        b
    }
}

/// Decodes a 32-bit AArch64 instruction word into a 100% sound [`InstInfo`].
///
/// Any opcode not explicitly verified returns [`InstInfo::conservative_barrier`].
pub const fn decode_inst(w: u32) -> InstInfo {
    // 0. NOP (`0xd503_201f`).
    if w == 0xd503_201f {
        return InstInfo {
            gpr_defs: 0,
            gpr_uses: 0,
            fp_defs: 0,
            fp_uses: 0,
            reads_flags: false,
            writes_flags: false,
            mem: MemEffect::None,
            is_branch_or_barrier: false,
            latency: 0,
            pipe: M5Pipe::Alu,
            kind: DecodedKind::Nop,
        };
    }

    // 1. Branches, calls, returns, traps.
    // `b imm26`: 000101 imm26
    if (w & 0xfc00_0000) == 0x1400_0000 {
        return InstInfo {
            gpr_defs: 0,
            gpr_uses: 0,
            fp_defs: 0,
            fp_uses: 0,
            reads_flags: false,
            writes_flags: false,
            mem: MemEffect::None,
            is_branch_or_barrier: true,
            latency: 1,
            pipe: M5Pipe::Branch,
            kind: DecodedKind::BranchUncond { delta_words: sext_bits(w & 0x3ff_ffff, 26) },
        };
    }
    // `bl imm26`: 100101 imm26
    if (w & 0xfc00_0000) == 0x9400_0000 {
        return InstInfo {
            gpr_defs: u32::MAX,
            gpr_uses: u32::MAX,
            fp_defs: u32::MAX,
            fp_uses: u32::MAX,
            reads_flags: true,
            writes_flags: true,
            mem: MemEffect::Barrier,
            is_branch_or_barrier: true,
            latency: 4,
            pipe: M5Pipe::Branch,
            kind: DecodedKind::Call,
        };
    }
    // `b.cond imm19`: 01010100 imm19 0 cond
    if (w & 0xff00_0010) == 0x5400_0000 {
        return InstInfo {
            gpr_defs: 0,
            gpr_uses: 0,
            fp_defs: 0,
            fp_uses: 0,
            reads_flags: true,
            writes_flags: false,
            mem: MemEffect::None,
            is_branch_or_barrier: true,
            latency: 1,
            pipe: M5Pipe::Branch,
            kind: DecodedKind::BranchCond {
                delta_words: sext_bits((w >> 5) & 0x7_ffff, 19),
                is_flag_cond: true,
                cond: u4_low(w),
            },
        };
    }
    // `cbz`/`cbnz`: sf 011010 op imm19 Rt
    if (w & 0x7e00_0000) == 0x3400_0000 {
        let rt = reg5(w);
        return InstInfo {
            gpr_defs: 0,
            gpr_uses: gpr_bit_zr(rt),
            fp_defs: 0,
            fp_uses: 0,
            reads_flags: false,
            writes_flags: false,
            mem: MemEffect::None,
            is_branch_or_barrier: true,
            latency: 1,
            pipe: M5Pipe::Branch,
            kind: DecodedKind::BranchCond {
                delta_words: sext_bits((w >> 5) & 0x7_ffff, 19),
                is_flag_cond: false,
                cond: 0,
            },
        };
    }
    // `tbz`/`tbnz`: b5 011011 op b40 imm14 Rt
    if (w & 0x7e00_0000) == 0x3600_0000 {
        let rt = reg5(w);
        return InstInfo {
            gpr_defs: 0,
            gpr_uses: gpr_bit_zr(rt),
            fp_defs: 0,
            fp_uses: 0,
            reads_flags: false,
            writes_flags: false,
            mem: MemEffect::None,
            is_branch_or_barrier: true,
            latency: 1,
            pipe: M5Pipe::Branch,
            kind: DecodedKind::BranchCond {
                delta_words: sext_bits((w >> 5) & 0x3fff, 14),
                is_flag_cond: false,
                cond: 0,
            },
        };
    }
    // `ret {xn}`: 11010110 01011111 000000 Rn 00000
    if (w & 0xffff_fc1f) == 0xd65f_0000 {
        let rn = reg5(w >> 5);
        return InstInfo {
            gpr_defs: 0,
            gpr_uses: RET_LIVE_GPRS | gpr_bit_zr(rn),
            fp_defs: 0,
            fp_uses: u32::MAX,
            reads_flags: true,
            writes_flags: false,
            mem: MemEffect::Barrier,
            is_branch_or_barrier: true,
            latency: 1,
            pipe: M5Pipe::Branch,
            kind: DecodedKind::Ret,
        };
    }
    // `blr xn`: 11010110 00111111 000000 Rn 00000
    if (w & 0xffff_fc1f) == 0xd63f_0000 {
        return InstInfo {
            gpr_defs: u32::MAX,
            gpr_uses: u32::MAX,
            fp_defs: u32::MAX,
            fp_uses: u32::MAX,
            reads_flags: true,
            writes_flags: true,
            mem: MemEffect::Barrier,
            is_branch_or_barrier: true,
            latency: 4,
            pipe: M5Pipe::Branch,
            kind: DecodedKind::Call,
        };
    }
    // `br xn`: 11010110 00011111 000000 Rn 00000
    if (w & 0xffff_fc1f) == 0xd61f_0000 {
        return InstInfo {
            gpr_defs: 0,
            gpr_uses: u32::MAX,
            fp_defs: 0,
            fp_uses: u32::MAX,
            reads_flags: true,
            writes_flags: false,
            mem: MemEffect::Barrier,
            is_branch_or_barrier: true,
            latency: 1,
            pipe: M5Pipe::Branch,
            kind: DecodedKind::IndirectBranch,
        };
    }
    // `brk #imm16`: 11010100 001 imm16 00000
    if (w & 0xffe0_001f) == 0xd420_0000 {
        return InstInfo::conservative_barrier();
    }

    // 2. Data processing — Immediate.
    // `adr` / `adrp`: op immlo 10000 immhi Rd -> PC-relative barrier!
    if (w & 0x1f00_0000) == 0x1000_0000 {
        let rd = reg5(w);
        return InstInfo {
            gpr_defs: gpr_bit_zr(rd),
            gpr_uses: 0,
            fp_defs: 0,
            fp_uses: 0,
            reads_flags: false,
            writes_flags: false,
            mem: MemEffect::Barrier,
            is_branch_or_barrier: true,
            latency: 1,
            pipe: M5Pipe::Alu,
            kind: DecodedKind::Other,
        };
    }
    // `add`/`sub`/`adds`/`subs` (immediate): sf op S 100010 sh imm12 Rn Rd
    // Note: `sh` in `0..=1` (excludes CSSC `minmax_imm` where bits 23:22 == 11).
    if (w & 0x1f80_0000) == 0x1100_0000 {
        let sh = (w >> 22) & 3;
        if sh <= 1 {
            let wide = (w >> 31) != 0;
            let is_sub = ((w >> 30) & 1) != 0;
            let sets_flags = ((w >> 29) & 1) != 0;
            let shift12 = sh == 1;
            let imm12 = u12_low(w >> 10);
            let rn = reg5(w >> 5);
            let rd = reg5(w);
            let gpr_uses = gpr_bit_sp(rn);
            let gpr_defs = if sets_flags { gpr_bit_zr(rd) } else { gpr_bit_sp(rd) };
            let kind = if is_sub && sets_flags && rd == 31 && imm12 == 0 && !shift12 && rn < 31 {
                DecodedKind::CmpZero { rn, wide }
            } else {
                DecodedKind::AddSubImm { rd, rn, imm12, shift12, is_sub, wide, sets_flags }
            };
            let writes_sp = !sets_flags && rd == 31;
            return InstInfo {
                gpr_defs,
                gpr_uses,
                fp_defs: 0,
                fp_uses: 0,
                reads_flags: false,
                writes_flags: sets_flags,
                mem: if writes_sp { MemEffect::Barrier } else { MemEffect::None },
                is_branch_or_barrier: false,
                latency: 1,
                pipe: M5Pipe::Alu,
                kind,
            };
        }
        return InstInfo::conservative_barrier();
    }
    // Logical (immediate): sf opc 100100 N immr imms Rn Rd
    if (w & 0x1f80_0000) == 0x1200_0000 {
        let wide = (w >> 31) != 0;
        let opc = (w >> 29) & 3;
        let n_bit = (w >> 22) & 1;
        if wide || n_bit == 0 {
            let rn = reg5(w >> 5);
            let rd = reg5(w);
            let sets_flags = opc == 3;
            let gpr_defs = if sets_flags { gpr_bit_zr(rd) } else { gpr_bit_sp(rd) };
            let writes_sp = !sets_flags && rd == 31;
            return InstInfo {
                gpr_defs,
                gpr_uses: gpr_bit_zr(rn),
                fp_defs: 0,
                fp_uses: 0,
                reads_flags: false,
                writes_flags: sets_flags,
                mem: if writes_sp { MemEffect::Barrier } else { MemEffect::None },
                is_branch_or_barrier: false,
                latency: 1,
                pipe: M5Pipe::Alu,
                kind: DecodedKind::Other,
            };
        }
        return InstInfo::conservative_barrier();
    }
    // Move wide immediate (`movn`/`movz`/`movk`): sf opc 100101 hw imm16 Rd
    if (w & 0x1f80_0000) == 0x1280_0000 {
        let wide = (w >> 31) != 0;
        let opc = (w >> 29) & 3;
        let hw = (w >> 21) & 3;
        if opc != 1 && (wide || hw < 2) {
            let rd = reg5(w);
            let rd_mask = gpr_bit_zr(rd);
            // `movk` (opc == 3) preserves the other 48 bits of Rd, so it ALSO reads Rd!
            let gpr_uses = if opc == 3 { rd_mask } else { 0 };
            return InstInfo::alu(rd_mask, gpr_uses, false, false, 1, DecodedKind::Other);
        }
        return InstInfo::conservative_barrier();
    }
    // Bitfield (`sbfm`/`bfm`/`ubfm`): sf opc 100110 N immr imms Rn Rd
    if (w & 0x1f80_0000) == 0x1300_0000 {
        let wide = (w >> 31) != 0;
        let opc = (w >> 29) & 3;
        let n_bit = ((w >> 22) & 1) != 0;
        if opc < 3 && n_bit == wide {
            let rn = reg5(w >> 5);
            let rd = reg5(w);
            let rd_mask = gpr_bit_zr(rd);
            // `bfm` (opc == 1, e.g. `bfxil`) preserves bits outside the field, so it ALSO reads Rd!
            let gpr_uses = gpr_bit_zr(rn) | if opc == 1 { rd_mask } else { 0 };
            return InstInfo::alu(rd_mask, gpr_uses, false, false, 1, DecodedKind::Other);
        }
        return InstInfo::conservative_barrier();
    }
    // Extract (`extr`): sf op21=00 100111 N o0=0 Rm imms Rn Rd
    if (w & 0x7fa0_0000) == 0x1380_0000 {
        let wide = (w >> 31) != 0;
        let n_bit = ((w >> 22) & 1) != 0;
        let imms = (w >> 10) & 0x3f;
        if n_bit == wide && (wide || imms < 32) {
            let rm = reg5(w >> 16);
            let rn = reg5(w >> 5);
            let rd = reg5(w);
            return InstInfo::alu(
                gpr_bit_zr(rd),
                gpr_bit_zr(rn) | gpr_bit_zr(rm),
                false,
                false,
                1,
                DecodedKind::Other,
            );
        }
        return InstInfo::conservative_barrier();
    }

    // 3. Data processing — Register.
    // Logical (shifted register): sf opc 01010 shift N Rm imm6 Rn Rd
    if (w & 0x1f00_0000) == 0x0a00_0000 {
        let wide = (w >> 31) != 0;
        let opc = (w >> 29) & 3;
        let imm6 = (w >> 10) & 0x3f;
        if wide || imm6 < 32 {
            let rm = reg5(w >> 16);
            let rn = reg5(w >> 5);
            let rd = reg5(w);
            let sets_flags = opc == 3;
            let kind = if (w & 0xffe0_ffe0) == 0xaa00_03e0 {
                // Pure 64-bit register move: `orr xd, xzr, xm, lsl #0`.
                DecodedKind::MovReg { rd, rn: rm, wide: true }
            } else if (w & 0xffe0_ffe0) == 0x2a00_03e0 {
                // 32-bit zero-extending register move: `orr wd, wzr, wm, lsl #0`.
                DecodedKind::MovReg { rd, rn: rm, wide: false }
            } else {
                DecodedKind::Other
            };
            return InstInfo::alu(
                gpr_bit_zr(rd),
                gpr_bit_zr(rn) | gpr_bit_zr(rm),
                false,
                sets_flags,
                1,
                kind,
            );
        }
        return InstInfo::conservative_barrier();
    }
    // Add/Sub (shifted register): sf op S 01011 shift 0 Rm imm6 Rn Rd
    if (w & 0x1f20_0000) == 0x0b00_0000 {
        let wide = (w >> 31) != 0;
        let is_sub = ((w >> 30) & 1) != 0;
        let sets_flags = ((w >> 29) & 1) != 0;
        let shift = (w >> 22) & 3;
        let imm6 = (w >> 10) & 0x3f;
        if shift != 3 && (wide || imm6 < 32) {
            let rm = reg5(w >> 16);
            let rn = reg5(w >> 5);
            let rd = reg5(w);
            let kind = if shift == 0 && imm6 == 0 {
                DecodedKind::AddSubReg { rd, rn, rm, is_sub, wide, sets_flags }
            } else {
                DecodedKind::Other
            };
            return InstInfo::alu(
                gpr_bit_zr(rd),
                gpr_bit_zr(rn) | gpr_bit_zr(rm),
                false,
                sets_flags,
                1,
                kind,
            );
        }
        return InstInfo::conservative_barrier();
    }
    // Add/Sub (extended register): sf op S 01011 00 1 Rm option imm3 Rn Rd
    if (w & 0x1fe0_0000) == 0x0b20_0000 {
        let sets_flags = ((w >> 29) & 1) != 0;
        let imm3 = (w >> 10) & 7;
        if imm3 <= 4 {
            let rm = reg5(w >> 16);
            let rn = reg5(w >> 5);
            let rd = reg5(w);
            let gpr_defs = if sets_flags { gpr_bit_zr(rd) } else { gpr_bit_sp(rd) };
            let writes_sp = !sets_flags && rd == 31;
            return InstInfo {
                gpr_defs,
                gpr_uses: gpr_bit_sp(rn) | gpr_bit_zr(rm),
                fp_defs: 0,
                fp_uses: 0,
                reads_flags: false,
                writes_flags: sets_flags,
                mem: if writes_sp { MemEffect::Barrier } else { MemEffect::None },
                is_branch_or_barrier: false,
                latency: 1,
                pipe: M5Pipe::Alu,
                kind: DecodedKind::Other,
            };
        }
        return InstInfo::conservative_barrier();
    }
    // Add/Sub with carry (`adc`/`adcs`/`sbc`/`sbcs`): sf op S 11010000 Rm 000000 Rn Rd
    if (w & 0x1fe0_fc00) == 0x1a00_0000 {
        let sets_flags = ((w >> 29) & 1) != 0;
        let rm = reg5(w >> 16);
        let rn = reg5(w >> 5);
        let rd = reg5(w);
        return InstInfo::alu(
            gpr_bit_zr(rd),
            gpr_bit_zr(rn) | gpr_bit_zr(rm),
            true,
            sets_flags,
            1,
            DecodedKind::Other,
        );
    }
    // Conditional compare (`ccmp`/`ccmn` reg/imm): sf op S=1 11010010 Rm/imm5 cond is_imm 0 Rn 0 nzcv
    if (w & 0x3fe0_0410) == 0x3a40_0000 {
        let is_imm = ((w >> 11) & 1) != 0;
        let rm = reg5(w >> 16);
        let rn = reg5(w >> 5);
        let uses = gpr_bit_zr(rn) | if is_imm { 0 } else { gpr_bit_zr(rm) };
        return InstInfo::alu(0, uses, true, true, 1, DecodedKind::Other);
    }
    // Conditional select (`csel`/`csinc`/`csinv`/`csneg`): sf op S=0 11010100 Rm cond op2 Rn Rd
    if (w & 0x3fe0_0800) == 0x1a80_0000 {
        let op2 = (w >> 10) & 3;
        if op2 <= 1 {
            let wide = (w >> 31) != 0;
            let op = (w >> 30) & 1;
            let rm = reg5(w >> 16);
            let cond = u4_low(w >> 12);
            let rn = reg5(w >> 5);
            let rd = reg5(w);
            let kind = if op == 0 && op2 == 0 {
                DecodedKind::Csel { rd, rn, rm, cond, wide }
            } else if op == 0 && op2 == 1 && rn == 31 && rm == 31 && cond < 14 {
                DecodedKind::Cset { rd, cond: cond ^ 1, wide }
            } else {
                DecodedKind::Other
            };
            return InstInfo::alu(
                gpr_bit_zr(rd),
                gpr_bit_zr(rn) | gpr_bit_zr(rm),
                true,
                false,
                1,
                kind,
            );
        }
        return InstInfo::conservative_barrier();
    }
    // Data-processing (3 source): `madd`/`msub`/`smaddl`/`umaddl`/`smulh`/`umulh`.
    if (w & 0x7f00_0000) == 0x1b00_0000 {
        let wide = (w >> 31) != 0;
        let op31 = (w >> 21) & 7;
        let o0 = (w >> 15) & 1;
        let rm = reg5(w >> 16);
        let ra = reg5(w >> 10);
        let rn = reg5(w >> 5);
        let rd = reg5(w);
        if op31 == 0 || (wide && (op31 == 1 || op31 == 5)) {
            return InstInfo::alu(
                gpr_bit_zr(rd),
                gpr_bit_zr(rn) | gpr_bit_zr(rm) | gpr_bit_zr(ra),
                false,
                false,
                3,
                DecodedKind::Other,
            );
        }
        if wide && (op31 == 2 || op31 == 6) && o0 == 0 {
            return InstInfo::alu(
                gpr_bit_zr(rd),
                gpr_bit_zr(rn) | gpr_bit_zr(rm),
                false,
                false,
                3,
                DecodedKind::Other,
            );
        }
        return InstInfo::conservative_barrier();
    }
    // Data-processing (2 source): `udiv`/`sdiv`/`lslv`/`lsrv`/`asrv`/`rorv`/`crc32*`/CSSC minmax.
    if (w & 0x7fe0_0000) == 0x1ac0_0000 {
        let opcode = (w >> 10) & 0x3f;
        let rm = reg5(w >> 16);
        let rn = reg5(w >> 5);
        let rd = reg5(w);
        let latency = match opcode {
            2 | 3 => 8,             // udiv, sdiv
            8..=11 | 24..=27 => 1,  // lslv, lsrv, asrv, rorv, cssc smax/umax/smin/umin
            16..=23 => 2,           // crc32*
            _ => return InstInfo::conservative_barrier(),
        };
        return InstInfo::alu(
            gpr_bit_zr(rd),
            gpr_bit_zr(rn) | gpr_bit_zr(rm),
            false,
            false,
            latency,
            DecodedKind::Other,
        );
    }
    // Data-processing (1 source): `rbit`/`rev16`/`rev32`/`rev`/`clz`/`cls`/CSSC `cnt`/`abs`.
    if (w & 0x7fe0_0000) == 0x5ac0_0000 {
        let opcode2 = (w >> 16) & 0x1f;
        let opcode = (w >> 10) & 0x3f;
        if opcode2 == 0 && (opcode <= 5 || opcode == 7 || opcode == 8) {
            let rn = reg5(w >> 5);
            let rd = reg5(w);
            return InstInfo::alu(gpr_bit_zr(rd), gpr_bit_zr(rn), false, false, 1, DecodedKind::Other);
        }
        return InstInfo::conservative_barrier();
    }

    // 4. Loads and Stores.
    // Unsigned-offset `ldr`/`str` (GPR & SIMD&FP): size 111 V 01 opc imm12 Rn Rt
    if (w & 0x3b00_0000) == 0x3900_0000 {
        return decode_ldst_unsigned_imm(w);
    }
    // Unscaled-offset `ldur`/`stur` (GPR & SIMD&FP): size 111 V 00 opc 0 imm9 00 Rn Rt
    if (w & 0x3b20_0c00) == 0x3800_0000 {
        return decode_ldst_unscaled_imm(w);
    }
    // Post-index / Pre-index `ldr`/`str` (GPR & SIMD&FP): size 111 V 00 opc 0 imm9 (01|11) Rn Rt
    if (w & 0x3b20_0c00) == 0x3800_0400 || (w & 0x3b20_0c00) == 0x3800_0c00 {
        return decode_ldst_writeback_imm(w);
    }
    // Register-offset `ldr`/`str` (GPR & SIMD&FP): size 111 V 00 opc 1 Rm option S 10 Rn Rt
    // Note: bits 11:10 == 10 cleanly excludes LSE atomics (bits 11:10 == 00).
    if (w & 0x3b20_0c00) == 0x3820_0800 {
        return decode_ldst_reg_off(w);
    }
    // Signed-offset `ldp`/`stp` (no writeback): opc 101 V 010 L imm7 Rt2 Rn Rt
    if (w & 0x3b80_0000) == 0x2900_0000 {
        return decode_ldp_stp(w, false);
    }
    // Pre-index / Post-index `ldp`/`stp`: opc 101 V (001 | 011) L imm7 Rt2 Rn Rt
    if (w & 0x3b80_0000) == 0x2880_0000 || (w & 0x3b80_0000) == 0x2980_0000 {
        return decode_ldp_stp(w, true);
    }

    // 5. Scalar Floating Point & Common NEON (from `enc.rs`).
    // `fmov` between GPR and S/D: `fmov_from_s` (0x1e26), `fmov_to_s` (0x1e27), `fmov_from_d` (0x9e66), `fmov_to_d` (0x9e67).
    if (w & 0xffff_fc00) == 0x1e26_0000 || (w & 0xffff_fc00) == 0x9e66_0000 {
        let rn = reg5(w >> 5);
        let rd = reg5(w);
        return InstInfo {
            gpr_defs: gpr_bit_zr(rd),
            gpr_uses: 0,
            fp_defs: 0,
            fp_uses: fp_bit(rn),
            reads_flags: false,
            writes_flags: false,
            mem: MemEffect::None,
            is_branch_or_barrier: false,
            latency: 2,
            pipe: M5Pipe::Fp,
            kind: DecodedKind::Other,
        };
    }
    if (w & 0xffff_fc00) == 0x1e27_0000 || (w & 0xffff_fc00) == 0x9e67_0000 {
        let rn = reg5(w >> 5);
        let rd = reg5(w);
        return InstInfo {
            gpr_defs: 0,
            gpr_uses: gpr_bit_zr(rn),
            fp_defs: fp_bit(rd),
            fp_uses: 0,
            reads_flags: false,
            writes_flags: false,
            mem: MemEffect::None,
            is_branch_or_barrier: false,
            latency: 2,
            pipe: M5Pipe::Fp,
            kind: DecodedKind::Other,
        };
    }
    // Scalar FP 2-source (`fmul`/`fdiv`/`fadd`/`fsub`): 00011110 ftype 1 Rm opcode 10 Rn Rd
    if (w & 0xffbf_0c00) == 0x1e20_0800 {
        let opcode = (w >> 12) & 0xf;
        if opcode <= 3 {
            let rm = reg5(w >> 16);
            let rn = reg5(w >> 5);
            let rd = reg5(w);
            return InstInfo {
                gpr_defs: 0,
                gpr_uses: 0,
                fp_defs: fp_bit(rd),
                fp_uses: fp_bit(rn) | fp_bit(rm),
                reads_flags: false,
                writes_flags: false,
                mem: MemEffect::None,
                is_branch_or_barrier: false,
                latency: if opcode == 1 { 8 } else { 3 },
                pipe: M5Pipe::Fp,
                kind: DecodedKind::Other,
            };
        }
    }
    // Scalar FP 1-source (`fmov`/`fabs`/`fneg`/`fsqrt`/`fcvt`/`frint*`): 00011110 ftype 1 opcode 10000 Rn Rd
    if (w & 0xff20_7c00) == 0x1e20_4000 {
        let ftype = (w >> 22) & 3;
        if ftype != 2 {
            let rn = reg5(w >> 5);
            let rd = reg5(w);
            return InstInfo {
                gpr_defs: 0,
                gpr_uses: 0,
                fp_defs: fp_bit(rd),
                fp_uses: fp_bit(rn),
                reads_flags: false,
                writes_flags: false,
                mem: MemEffect::None,
                is_branch_or_barrier: false,
                latency: 2,
                pipe: M5Pipe::Fp,
                kind: DecodedKind::Other,
            };
        }
    }
    // Scalar FP 3-source (`fmadd`/`fmsub`): 00011111 ftype 0 Rm o0 Ra Rn Rd
    if (w & 0xff20_0000) == 0x1f00_0000 {
        let ftype = (w >> 22) & 3;
        if ftype != 2 {
            let rm = reg5(w >> 16);
            let ra = reg5(w >> 10);
            let rn = reg5(w >> 5);
            let rd = reg5(w);
            return InstInfo {
                gpr_defs: 0,
                gpr_uses: 0,
                fp_defs: fp_bit(rd),
                fp_uses: fp_bit(rn) | fp_bit(rm) | fp_bit(ra),
                reads_flags: false,
                writes_flags: false,
                mem: MemEffect::None,
                is_branch_or_barrier: false,
                latency: 3,
                pipe: M5Pipe::Fp,
                kind: DecodedKind::Other,
            };
        }
    }
    // Scalar `fcmp`: 00011110 ftype 1 Rm 00 1000 Rn 00000
    if (w & 0xffbf_fc1f) == 0x1e20_2000 {
        let rm = reg5(w >> 16);
        let rn = reg5(w >> 5);
        return InstInfo {
            gpr_defs: 0,
            gpr_uses: 0,
            fp_defs: 0,
            fp_uses: fp_bit(rn) | fp_bit(rm),
            reads_flags: false,
            writes_flags: true,
            mem: MemEffect::None,
            is_branch_or_barrier: false,
            latency: 2,
            pipe: M5Pipe::Fp,
            kind: DecodedKind::Other,
        };
    }
    // Scalar `fcsel`: 00011110 ftype 1 Rm cond 11 Rn Rd
    if (w & 0xffbf_0c00) == 0x1e20_0c00 {
        let rm = reg5(w >> 16);
        let rn = reg5(w >> 5);
        let rd = reg5(w);
        return InstInfo {
            gpr_defs: 0,
            gpr_uses: 0,
            fp_defs: fp_bit(rd),
            fp_uses: fp_bit(rn) | fp_bit(rm),
            reads_flags: true,
            writes_flags: false,
            mem: MemEffect::None,
            is_branch_or_barrier: false,
            latency: 2,
            pipe: M5Pipe::Fp,
            kind: DecodedKind::Other,
        };
    }
    // `fcvtzs` / `fcvtzu` (FP -> GPR).
    if (w & 0x7fbe_fc00) == 0x1e38_0000 {
        let rn = reg5(w >> 5);
        let rd = reg5(w);
        return InstInfo {
            gpr_defs: gpr_bit_zr(rd),
            gpr_uses: 0,
            fp_defs: 0,
            fp_uses: fp_bit(rn),
            reads_flags: false,
            writes_flags: false,
            mem: MemEffect::None,
            is_branch_or_barrier: false,
            latency: 3,
            pipe: M5Pipe::Fp,
            kind: DecodedKind::Other,
        };
    }
    // `scvtf` / `ucvtf` (GPR -> FP).
    if (w & 0x7fbe_fc00) == 0x1e22_0000 {
        let rn = reg5(w >> 5);
        let rd = reg5(w);
        return InstInfo {
            gpr_defs: 0,
            gpr_uses: gpr_bit_zr(rn),
            fp_defs: fp_bit(rd),
            fp_uses: 0,
            reads_flags: false,
            writes_flags: false,
            mem: MemEffect::None,
            is_branch_or_barrier: false,
            latency: 3,
            pipe: M5Pipe::Fp,
            kind: DecodedKind::Other,
        };
    }
    // Common 128-bit NEON vector arithmetic (`add_v`/`sub_v`/`mul_v`/`fadd_v`/`fsub_v`/`fmul_v`/`orr_v`/`eor_v`).
    if (w & 0xdf20_fc00) == 0x4e20_8400
        || (w & 0xff20_fc00) == 0x4e20_9c00
        || (w & 0xff3f_fc00) == 0x4e20_d400
        || (w & 0xff3f_fc00) == 0x4ea0_d400
        || (w & 0xffbf_fc00) == 0x6e20_dc00
        || (w & 0xffe0_fc00) == 0x4ea0_1c00
        || (w & 0xffe0_fc00) == 0x6e20_1c00
    {
        let rm = reg5(w >> 16);
        let rn = reg5(w >> 5);
        let rd = reg5(w);
        return InstInfo {
            gpr_defs: 0,
            gpr_uses: 0,
            fp_defs: fp_bit(rd),
            fp_uses: fp_bit(rn) | fp_bit(rm),
            reads_flags: false,
            writes_flags: false,
            mem: MemEffect::None,
            is_branch_or_barrier: false,
            latency: 3,
            pipe: M5Pipe::Fp,
            kind: DecodedKind::Other,
        };
    }
    // Accumulating 128-bit NEON (`fmla_v`, `fmls_v`, `mla_v`, `mls_v`): READS AND WRITES `rd`!
    if (w & 0xff3f_fc00) == 0x4e20_cc00
        || (w & 0xff3f_fc00) == 0x4ea0_cc00
        || (w & 0xdf20_fc00) == 0x4e20_9400
    {
        let rm = reg5(w >> 16);
        let rn = reg5(w >> 5);
        let rd = reg5(w);
        return InstInfo {
            gpr_defs: 0,
            gpr_uses: 0,
            fp_defs: fp_bit(rd),
            fp_uses: fp_bit(rn) | fp_bit(rm) | fp_bit(rd),
            reads_flags: false,
            writes_flags: false,
            mem: MemEffect::None,
            is_branch_or_barrier: false,
            latency: 3,
            pipe: M5Pipe::Fp,
            kind: DecodedKind::Other,
        };
    }

    // Anything else (LSE atomics, exclusives, fences, SME2, system regs, inline asm, UDF, etc.):
    // full conservative barrier.
    InstInfo::conservative_barrier()
}

const fn decode_ldst_unsigned_imm(w: u32) -> InstInfo {
    let size_bits = (w >> 30) & 3;
    let is_fp = ((w >> 26) & 1) != 0;
    let opc = (w >> 22) & 3;
    let imm12 = ((w >> 10) & 0xfff).cast_signed();
    let rn = reg5(w >> 5);
    let rt = reg5(w);

    if !is_fp {
        // `prfm`: size == 11 && opc == 10 (prefetch hint — keep as non-writing LdSt, not LdrOff).
        if size_bits == 3 && opc == 2 {
            return InstInfo {
                gpr_defs: 0,
                gpr_uses: gpr_bit_sp(rn),
                fp_defs: 0,
                fp_uses: 0,
                reads_flags: false,
                writes_flags: false,
                mem: MemEffect::Load,
                is_branch_or_barrier: false,
                latency: 1,
                pipe: M5Pipe::LdSt,
                kind: DecodedKind::Other,
            };
        }
        let Some(size) = mem_size_from_bits(size_bits) else {
            return InstInfo::conservative_barrier();
        };
        let imm_bytes = imm12 * size.bytes();
        match opc {
            0 => InstInfo {
                gpr_defs: 0,
                gpr_uses: gpr_bit_sp(rn) | gpr_bit_zr(rt),
                fp_defs: 0,
                fp_uses: 0,
                reads_flags: false,
                writes_flags: false,
                mem: MemEffect::Store,
                is_branch_or_barrier: false,
                latency: 1,
                pipe: M5Pipe::LdSt,
                kind: DecodedKind::StrOff { rt, rn, imm_bytes, size, is_fp: false },
            },
            1 => InstInfo {
                gpr_defs: gpr_bit_zr(rt),
                gpr_uses: gpr_bit_sp(rn),
                fp_defs: 0,
                fp_uses: 0,
                reads_flags: false,
                writes_flags: false,
                mem: MemEffect::Load,
                is_branch_or_barrier: false,
                latency: 4,
                pipe: M5Pipe::LdSt,
                kind: DecodedKind::LdrOff { rt, rn, imm_bytes, size, is_fp: false },
            },
            // Signed loads (`ldrsb`, `ldrsh`, `ldrsw`): valid for `size < 3` (opc 2) or `size < 2` (opc 3).
            2 | 3 if (opc == 2 && size_bits < 3) || (opc == 3 && size_bits < 2) => InstInfo {
                gpr_defs: gpr_bit_zr(rt),
                gpr_uses: gpr_bit_sp(rn),
                fp_defs: 0,
                fp_uses: 0,
                reads_flags: false,
                writes_flags: false,
                mem: MemEffect::Load,
                is_branch_or_barrier: false,
                latency: 4,
                pipe: M5Pipe::LdSt,
                kind: DecodedKind::Other,
            },
            _ => InstInfo::conservative_barrier(),
        }
    } else {
        // SIMD&FP unsigned offset: `size=00 && opc in {2,3}` is 128-bit `Q`.
        let (is_load, size) = match (size_bits, opc) {
            (0, 0) => (false, MemSize::B8),
            (1, 0) => (false, MemSize::H16),
            (2, 0) => (false, MemSize::W32),
            (3, 0) => (false, MemSize::X64),
            (0, 1) => (true, MemSize::B8),
            (1, 1) => (true, MemSize::H16),
            (2, 1) => (true, MemSize::W32),
            (3, 1) => (true, MemSize::X64),
            (0, 2) => (false, MemSize::Q128),
            (0, 3) => (true, MemSize::Q128),
            _ => return InstInfo::conservative_barrier(),
        };
        let imm_bytes = imm12 * size.bytes();
        if is_load {
            InstInfo {
                gpr_defs: 0,
                gpr_uses: gpr_bit_sp(rn),
                fp_defs: fp_bit(rt),
                fp_uses: 0,
                reads_flags: false,
                writes_flags: false,
                mem: MemEffect::Load,
                is_branch_or_barrier: false,
                latency: 4,
                pipe: M5Pipe::LdSt,
                kind: DecodedKind::LdrOff { rt, rn, imm_bytes, size, is_fp: true },
            }
        } else {
            InstInfo {
                gpr_defs: 0,
                gpr_uses: gpr_bit_sp(rn),
                fp_defs: 0,
                fp_uses: fp_bit(rt),
                reads_flags: false,
                writes_flags: false,
                mem: MemEffect::Store,
                is_branch_or_barrier: false,
                latency: 1,
                pipe: M5Pipe::LdSt,
                kind: DecodedKind::StrOff { rt, rn, imm_bytes, size, is_fp: true },
            }
        }
    }
}

const fn decode_ldst_unscaled_imm(w: u32) -> InstInfo {
    let size_bits = (w >> 30) & 3;
    let is_fp = ((w >> 26) & 1) != 0;
    let opc = (w >> 22) & 3;
    let imm_bytes = sext_bits((w >> 12) & 0x1ff, 9);
    let rn = reg5(w >> 5);
    let rt = reg5(w);

    if !is_fp {
        let Some(size) = mem_size_from_bits(size_bits) else {
            return InstInfo::conservative_barrier();
        };
        match opc {
            0 => InstInfo {
                gpr_defs: 0,
                gpr_uses: gpr_bit_sp(rn) | gpr_bit_zr(rt),
                fp_defs: 0,
                fp_uses: 0,
                reads_flags: false,
                writes_flags: false,
                mem: MemEffect::Store,
                is_branch_or_barrier: false,
                latency: 1,
                pipe: M5Pipe::LdSt,
                kind: DecodedKind::StrOff { rt, rn, imm_bytes, size, is_fp: false },
            },
            1 => InstInfo {
                gpr_defs: gpr_bit_zr(rt),
                gpr_uses: gpr_bit_sp(rn),
                fp_defs: 0,
                fp_uses: 0,
                reads_flags: false,
                writes_flags: false,
                mem: MemEffect::Load,
                is_branch_or_barrier: false,
                latency: 4,
                pipe: M5Pipe::LdSt,
                kind: DecodedKind::LdrOff { rt, rn, imm_bytes, size, is_fp: false },
            },
            _ => InstInfo::conservative_barrier(),
        }
    } else {
        let (is_load, size) = match (size_bits, opc) {
            (0, 0) => (false, MemSize::B8),
            (1, 0) => (false, MemSize::H16),
            (2, 0) => (false, MemSize::W32),
            (3, 0) => (false, MemSize::X64),
            (0, 1) => (true, MemSize::B8),
            (1, 1) => (true, MemSize::H16),
            (2, 1) => (true, MemSize::W32),
            (3, 1) => (true, MemSize::X64),
            (0, 2) => (false, MemSize::Q128),
            (0, 3) => (true, MemSize::Q128),
            _ => return InstInfo::conservative_barrier(),
        };
        if is_load {
            InstInfo {
                gpr_defs: 0,
                gpr_uses: gpr_bit_sp(rn),
                fp_defs: fp_bit(rt),
                fp_uses: 0,
                reads_flags: false,
                writes_flags: false,
                mem: MemEffect::Load,
                is_branch_or_barrier: false,
                latency: 4,
                pipe: M5Pipe::LdSt,
                kind: DecodedKind::LdrOff { rt, rn, imm_bytes, size, is_fp: true },
            }
        } else {
            InstInfo {
                gpr_defs: 0,
                gpr_uses: gpr_bit_sp(rn),
                fp_defs: 0,
                fp_uses: fp_bit(rt),
                reads_flags: false,
                writes_flags: false,
                mem: MemEffect::Store,
                is_branch_or_barrier: false,
                latency: 1,
                pipe: M5Pipe::LdSt,
                kind: DecodedKind::StrOff { rt, rn, imm_bytes, size, is_fp: true },
            }
        }
    }
}

const fn decode_ldst_writeback_imm(w: u32) -> InstInfo {
    let size_bits = (w >> 30) & 3;
    let is_fp = ((w >> 26) & 1) != 0;
    let opc = (w >> 22) & 3;
    let rn = reg5(w >> 5);
    let rt = reg5(w);
    let wb_mask = gpr_bit_sp(rn);
    let sp_wb = rn == 31;

    if !is_fp {
        match opc {
            0 => InstInfo {
                gpr_defs: wb_mask,
                gpr_uses: gpr_bit_sp(rn) | gpr_bit_zr(rt),
                fp_defs: 0,
                fp_uses: 0,
                reads_flags: false,
                writes_flags: false,
                mem: if sp_wb { MemEffect::Barrier } else { MemEffect::Store },
                is_branch_or_barrier: sp_wb,
                latency: 1,
                pipe: M5Pipe::LdSt,
                kind: DecodedKind::Other,
            },
            1 | 2 | 3 if opc == 1 || (opc == 2 && size_bits < 3) || (opc == 3 && size_bits < 2) => InstInfo {
                gpr_defs: gpr_bit_zr(rt) | wb_mask,
                gpr_uses: gpr_bit_sp(rn),
                fp_defs: 0,
                fp_uses: 0,
                reads_flags: false,
                writes_flags: false,
                mem: if sp_wb { MemEffect::Barrier } else { MemEffect::Load },
                is_branch_or_barrier: sp_wb,
                latency: 4,
                pipe: M5Pipe::LdSt,
                kind: DecodedKind::Other,
            },
            _ => InstInfo::conservative_barrier(),
        }
    } else {
        let is_load = match (size_bits, opc) {
            (_, 0) | (0, 2) => false,
            (_, 1) | (0, 3) => true,
            _ => return InstInfo::conservative_barrier(),
        };
        if is_load {
            InstInfo {
                gpr_defs: wb_mask,
                gpr_uses: gpr_bit_sp(rn),
                fp_defs: fp_bit(rt),
                fp_uses: 0,
                reads_flags: false,
                writes_flags: false,
                mem: if sp_wb { MemEffect::Barrier } else { MemEffect::Load },
                is_branch_or_barrier: sp_wb,
                latency: 4,
                pipe: M5Pipe::LdSt,
                kind: DecodedKind::Other,
            }
        } else {
            InstInfo {
                gpr_defs: wb_mask,
                gpr_uses: gpr_bit_sp(rn),
                fp_defs: 0,
                fp_uses: fp_bit(rt),
                reads_flags: false,
                writes_flags: false,
                mem: if sp_wb { MemEffect::Barrier } else { MemEffect::Store },
                is_branch_or_barrier: sp_wb,
                latency: 1,
                pipe: M5Pipe::LdSt,
                kind: DecodedKind::Other,
            }
        }
    }
}

const fn decode_ldst_reg_off(w: u32) -> InstInfo {
    let option = (w >> 13) & 7;
    // Valid index extend options are UXTW (2), LSL (3), SXTW (6), SXTX (7).
    if option != 2 && option != 3 && option != 6 && option != 7 {
        return InstInfo::conservative_barrier();
    }
    let size_bits = (w >> 30) & 3;
    let is_fp = ((w >> 26) & 1) != 0;
    let opc = (w >> 22) & 3;
    let rm = reg5(w >> 16);
    let rn = reg5(w >> 5);
    let rt = reg5(w);

    if !is_fp {
        match opc {
            0 => InstInfo {
                gpr_defs: 0,
                gpr_uses: gpr_bit_sp(rn) | gpr_bit_zr(rm) | gpr_bit_zr(rt),
                fp_defs: 0,
                fp_uses: 0,
                reads_flags: false,
                writes_flags: false,
                mem: MemEffect::Store,
                is_branch_or_barrier: false,
                latency: 1,
                pipe: M5Pipe::LdSt,
                kind: DecodedKind::Other,
            },
            1 | 2 | 3 if opc == 1 || (opc == 2 && size_bits < 3) || (opc == 3 && size_bits < 2) => {
                InstInfo {
                    gpr_defs: gpr_bit_zr(rt),
                    gpr_uses: gpr_bit_sp(rn) | gpr_bit_zr(rm),
                    fp_defs: 0,
                    fp_uses: 0,
                    reads_flags: false,
                    writes_flags: false,
                    mem: MemEffect::Load,
                    is_branch_or_barrier: false,
                    latency: 4,
                    pipe: M5Pipe::LdSt,
                    kind: DecodedKind::Other,
                }
            }
            _ => InstInfo::conservative_barrier(),
        }
    } else {
        let is_load = match (size_bits, opc) {
            (_, 0) | (0, 2) => false,
            (_, 1) | (0, 3) => true,
            _ => return InstInfo::conservative_barrier(),
        };
        if is_load {
            InstInfo {
                gpr_defs: 0,
                gpr_uses: gpr_bit_sp(rn) | gpr_bit_zr(rm),
                fp_defs: fp_bit(rt),
                fp_uses: 0,
                reads_flags: false,
                writes_flags: false,
                mem: MemEffect::Load,
                is_branch_or_barrier: false,
                latency: 4,
                pipe: M5Pipe::LdSt,
                kind: DecodedKind::Other,
            }
        } else {
            InstInfo {
                gpr_defs: 0,
                gpr_uses: gpr_bit_sp(rn) | gpr_bit_zr(rm),
                fp_defs: 0,
                fp_uses: fp_bit(rt),
                reads_flags: false,
                writes_flags: false,
                mem: MemEffect::Store,
                is_branch_or_barrier: false,
                latency: 1,
                pipe: M5Pipe::LdSt,
                kind: DecodedKind::Other,
            }
        }
    }
}

const fn decode_ldp_stp(w: u32, writeback: bool) -> InstInfo {
    let opc = (w >> 30) & 3;
    let is_fp = ((w >> 26) & 1) != 0;
    let is_load = ((w >> 22) & 1) != 0;
    let rt2 = reg5(w >> 10);
    let rn = reg5(w >> 5);
    let rt = reg5(w);

    if !is_fp {
        if opc == 3 || (opc == 1 && !is_load) {
            return InstInfo::conservative_barrier();
        }
        let wb_mask = if writeback { gpr_bit_sp(rn) } else { 0 };
        let sp_wb = writeback && rn == 31;
        if is_load {
            InstInfo {
                gpr_defs: gpr_bit_zr(rt) | gpr_bit_zr(rt2) | wb_mask,
                gpr_uses: gpr_bit_sp(rn),
                fp_defs: 0,
                fp_uses: 0,
                reads_flags: false,
                writes_flags: false,
                mem: if sp_wb { MemEffect::Barrier } else { MemEffect::Load },
                is_branch_or_barrier: sp_wb,
                latency: 4,
                pipe: M5Pipe::LdSt,
                kind: DecodedKind::Other,
            }
        } else {
            InstInfo {
                gpr_defs: wb_mask,
                gpr_uses: gpr_bit_sp(rn) | gpr_bit_zr(rt) | gpr_bit_zr(rt2),
                fp_defs: 0,
                fp_uses: 0,
                reads_flags: false,
                writes_flags: false,
                mem: if sp_wb { MemEffect::Barrier } else { MemEffect::Store },
                is_branch_or_barrier: sp_wb,
                latency: 1,
                pipe: M5Pipe::LdSt,
                kind: DecodedKind::Other,
            }
        }
    } else {
        if opc == 3 {
            return InstInfo::conservative_barrier();
        }
        let wb_mask = if writeback { gpr_bit_sp(rn) } else { 0 };
        let sp_wb = writeback && rn == 31;
        if is_load {
            InstInfo {
                gpr_defs: wb_mask,
                gpr_uses: gpr_bit_sp(rn),
                fp_defs: fp_bit(rt) | fp_bit(rt2),
                fp_uses: 0,
                reads_flags: false,
                writes_flags: false,
                mem: if sp_wb { MemEffect::Barrier } else { MemEffect::Load },
                is_branch_or_barrier: sp_wb,
                latency: 4,
                pipe: M5Pipe::LdSt,
                kind: DecodedKind::Other,
            }
        } else {
            InstInfo {
                gpr_defs: wb_mask,
                gpr_uses: gpr_bit_sp(rn),
                fp_defs: 0,
                fp_uses: fp_bit(rt) | fp_bit(rt2),
                reads_flags: false,
                writes_flags: false,
                mem: if sp_wb { MemEffect::Barrier } else { MemEffect::Store },
                is_branch_or_barrier: sp_wb,
                latency: 1,
                pipe: M5Pipe::LdSt,
                kind: DecodedKind::Other,
            }
        }
    }
}

#[inline]
const fn mem_size_from_bits(bits: u32) -> Option<MemSize> {
    match bits {
        0 => Some(MemSize::B8),
        1 => Some(MemSize::H16),
        2 => Some(MemSize::W32),
        3 => Some(MemSize::X64),
        _ => None,
    }
}

// =============================================================================
// 2. Public `MachOptimizeInput` / `MachOptimizeOut` / `MachStats` API
// =============================================================================

/// Counters for transformations applied by [`optimize_mach_a64`].
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct MachStats {
    pub movs_eliminated: u32,
    pub pairs_fused: u32,
    pub insts_scheduled: u32,
    pub branches_elided: u32,
}

/// Output returned by [`optimize_mach_a64`].
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct MachOptimizeOut {
    pub stats: MachStats,
}

/// Input state for the AArch64 post-RA machine IR optimizer ([`optimize_mach_a64`]).
pub struct MachOptimizeInput<'a, 'b> {
    /// Emitted 32-bit AArch64 instruction words BEFORE final `BranchPatch` offsets are resolved.
    pub words: &'b mut Vec<u32, Alloc<'a>>,
    /// Start word offset of each block `0..num_blocks` (remapped in place on return).
    pub block_word_off: &'b mut [usize],
    /// Emitted block IDs in code-layout order.
    pub block_order: &'b [usize],
    /// Word indices of `BranchPatch` instructions to keep pinned & remapped in place
    /// (set to [`ELIDED_PATCH`] if a fallthrough unconditional `b` is elided).
    pub patch_word_idxs: &'b mut [usize],
    /// Optional parallel target-block IDs for `patch_word_idxs` (either empty `&[]` or same
    /// length as `patch_word_idxs`) so CFG liveness and fallthrough elision know exact targets
    /// before final `BranchPatch` resolution.
    pub patch_target_blocks: &'b [u32],
    /// Byte offsets of relocations — pinned as barriers & remapped in place.
    pub reloc_byte_offs: &'b mut [u32],
    /// Byte offsets of traps — pinned as barriers & remapped in place.
    pub trap_byte_offs: &'b mut [u32],
    /// Optional pinned internal branch targets (e.g. cold edge stubs) starting sub-region boundaries.
    pub cold_stub_word_idxs: &'b [usize],
    /// Additional pinned word indices (e.g. `shared_jump_tables` `adr_sites`, `sme2_*_bl_sites`,
    /// and `pending_unwind` call sites) resolved or consumed by the caller AFTER `optimize_mach_a64`.
    pub extra_word_idxs: &'b mut [usize],
    /// Optional per-block 16-byte alignment flags (either empty `&[]` or length `block_word_off.len()`).
    /// When `align16_blocks[b]` is `true`, block `b` is padded with NOPs before entry so
    /// `block_word_off[b] % 4 == 0` in the final emitted code.
    pub align16_blocks: &'b [bool],
    /// Word indices emitted for `Flags::VOLATILE` loads/stores — pinned as memory barriers so
    /// post-RA pairing (`ldp`/`stp`) and scheduling never coalesce or reorder volatile accesses.
    pub volatile_word_idxs: &'b [usize],
    /// Exact bitmask of GPRs live-in at `ret` (e.g. [`RET_CALLEE_SAVED_GPRS`] for void/FP return,
    /// `RET_CALLEE_SAVED_GPRS | 1` for 1-GPR `X0` return, `RET_CALLEE_SAVED_GPRS | 3` for 2-GPR
    /// `X0:X1`/`i128` return, or [`RET_LIVE_GPRS`] conservatively). `0` defaults to [`RET_LIVE_GPRS`].
    pub ret_live_gprs: u32,
    /// Explicit arena/frame allocator for all scratch SoA buffers.
    pub alloc: Alloc<'a>,
}

/// Runs the post-RA AArch64 machine optimizer in place on `input`.
pub fn optimize_mach_a64<'a>(input: &mut MachOptimizeInput<'a, '_>) -> MachOptimizeOut {
    let mut stats = MachStats::default();
    if input.words.is_empty() || input.block_order.is_empty() {
        return MachOptimizeOut { stats };
    }
    if !validate_block_layout(input.words.len(), input.block_word_off, input.block_order) {
        return MachOptimizeOut { stats };
    }

    let alloc = input.alloc;
    let mut ctx = MachContext::build(input, alloc);

    // Pass 1: Peephole (self/dead mov, flag fusion, live-range anti-dep csel rename, copy-prop).
    mach_peephole_a64(&mut ctx, &mut stats);
    ctx.compact_and_remap();

    // Pass 1b: Intra-MBB redundant load elimination & store-to-load forwarding (unlocks LDP pairing).
    mach_redundant_loads(&mut ctx, &mut stats);
    ctx.compact_and_remap();

    // Pass 2: Cross-ALU non-aliasing LDP/STP pairing.
    mach_pair_ldp_stp(&mut ctx, &mut stats);
    ctx.compact_and_remap();

    // Pass 2b: Post-index load/store + pointer-increment fusion.
    mach_fuse_post_index(&mut ctx, &mut stats);
    ctx.compact_and_remap();

    // Pass 3: Post-RA Apple M5 / Graviton 5 list scheduler with macro-op fusion lock.
    mach_sched_m5(&mut ctx, &mut stats);

    // Pass 4: Fallthrough unconditional branch elision + exact offset remapping.
    mach_branch_opt(&mut ctx, &mut stats);
    ctx.compact_and_remap();

    // Final pass: Exact 16-byte loop-header alignment + offset/branch remapping.
    ctx.realign_loop_headers(input.align16_blocks);

    // Write optimized words and remapped offsets back to caller's buffers.
    ctx.write_back(input);

    MachOptimizeOut { stats }
}

fn validate_block_layout(words_len: usize, block_word_off: &[usize], block_order: &[usize]) -> bool {
    let mut prev = 0usize;
    for &b in block_order {
        let Some(&off) = block_word_off.get(b) else {
            return false;
        };
        if off > words_len || off < prev {
            return false;
        }
        prev = off;
    }
    for &off in block_word_off {
        if off > words_len {
            return false;
        }
    }
    true
}

// =============================================================================
// 3. SoA Decoded Machine Function & Compaction / Remapping Context
// =============================================================================

/// A pre-resolved PC-relative intra-function branch whose displacement must be updated
/// whenever instructions between `src_word` and `dst_word` are deleted.
#[derive(Clone, Copy, Debug)]
struct InternalBranch {
    src_word: usize,
    dst_word: usize,
    field: Field,
}

/// Structure-of-Arrays decoded AArch64 instruction table.
struct MachFuncSoa<'a> {
    words: Vec<u32, Alloc<'a>>,
    gpr_defs: Vec<u32, Alloc<'a>>,
    gpr_uses: Vec<u32, Alloc<'a>>,
    fp_defs: Vec<u32, Alloc<'a>>,
    fp_uses: Vec<u32, Alloc<'a>>,
    flags_rw: Vec<u8, Alloc<'a>>,
    mem: Vec<MemEffect, Alloc<'a>>,
    latency: Vec<u8, Alloc<'a>>,
    pipe: Vec<M5Pipe, Alloc<'a>>,
    kind: Vec<DecodedKind, Alloc<'a>>,
    state: Vec<u8, Alloc<'a>>,
    ret_live_gprs: u32,
}

impl<'a> MachFuncSoa<'a> {
    fn from_words(words: &[u32], ret_live_gprs: u32, alloc: Alloc<'a>) -> Self {
        let n = words.len();
        let mut soa = Self {
            words: Vec::with_capacity_in(n, alloc),
            gpr_defs: Vec::with_capacity_in(n, alloc),
            gpr_uses: Vec::with_capacity_in(n, alloc),
            fp_defs: Vec::with_capacity_in(n, alloc),
            fp_uses: Vec::with_capacity_in(n, alloc),
            flags_rw: Vec::with_capacity_in(n, alloc),
            mem: Vec::with_capacity_in(n, alloc),
            latency: Vec::with_capacity_in(n, alloc),
            pipe: Vec::with_capacity_in(n, alloc),
            kind: Vec::with_capacity_in(n, alloc),
            state: Vec::with_capacity_in(n, alloc),
            ret_live_gprs,
        };
        for &w in words {
            let info = decode_inst(w);
            let gpr_uses = if matches!(info.kind, DecodedKind::Ret) {
                ret_live_gprs | gpr_bit_zr(reg5(w >> 5))
            } else {
                info.gpr_uses
            };
            soa.words.push(w);
            soa.gpr_defs.push(info.gpr_defs);
            soa.gpr_uses.push(gpr_uses);
            soa.fp_defs.push(info.fp_defs);
            soa.fp_uses.push(info.fp_uses);
            soa.flags_rw.push(info.flags_byte());
            soa.mem.push(info.mem);
            soa.latency.push(info.latency);
            soa.pipe.push(info.pipe);
            soa.kind.push(info.kind);
            soa.state.push(0);
        }
        soa
    }

    #[inline]
    fn len(&self) -> usize {
        self.words.len()
    }

    #[inline]
    fn replace_word(&mut self, idx: usize, new_word: u32) {
        let info = decode_inst(new_word);
        let gpr_uses = if matches!(info.kind, DecodedKind::Ret) {
            self.ret_live_gprs | gpr_bit_zr(reg5(new_word >> 5))
        } else {
            info.gpr_uses
        };
        set_at(&mut self.words, idx, new_word);
        set_at(&mut self.gpr_defs, idx, info.gpr_defs);
        set_at(&mut self.gpr_uses, idx, gpr_uses);
        set_at(&mut self.fp_defs, idx, info.fp_defs);
        set_at(&mut self.fp_uses, idx, info.fp_uses);
        set_at(&mut self.flags_rw, idx, info.flags_byte());
        set_at(&mut self.mem, idx, info.mem);
        set_at(&mut self.latency, idx, info.latency);
        set_at(&mut self.pipe, idx, info.pipe);
        set_at(&mut self.kind, idx, info.kind);
    }

    #[inline]
    fn is_deleted(&self, idx: usize) -> bool {
        (at_u8(&self.state, idx) & STATE_DELETED) != 0
    }

    #[inline]
    fn is_pinned(&self, idx: usize) -> bool {
        (at_u8(&self.state, idx) & STATE_PINNED) != 0
    }

    #[inline]
    fn is_reloc_or_trap(&self, idx: usize) -> bool {
        (at_u8(&self.state, idx) & STATE_RELOC_OR_TRAP) != 0
    }

    #[inline]
    fn is_target_boundary(&self, idx: usize) -> bool {
        (at_u8(&self.state, idx) & STATE_TARGET_BOUNDARY) != 0
    }

    #[inline]
    fn is_opaque_data(&self, idx: usize) -> bool {
        (at_u8(&self.state, idx) & STATE_OPAQUE_DATA) != 0
    }

    #[inline]
    fn reads_flags(&self, idx: usize) -> bool {
        (at_u8(&self.flags_rw, idx) & FLAG_READS_NZCV) != 0
    }

    #[inline]
    fn writes_flags(&self, idx: usize) -> bool {
        (at_u8(&self.flags_rw, idx) & FLAG_WRITES_NZCV) != 0
    }

    #[inline]
    fn is_branch_or_barrier(&self, idx: usize) -> bool {
        (at_u8(&self.flags_rw, idx) & FLAG_BRANCH_OR_BARRIER) != 0
    }

    #[inline]
    fn kind_at(&self, idx: usize) -> DecodedKind {
        self.kind.get(idx).copied().unwrap_or(DecodedKind::Other)
    }

    #[inline]
    fn mem_at(&self, idx: usize) -> MemEffect {
        self.mem.get(idx).copied().unwrap_or(MemEffect::Barrier)
    }

    #[inline]
    fn pipe_at(&self, idx: usize) -> M5Pipe {
        self.pipe.get(idx).copied().unwrap_or(M5Pipe::Alu)
    }

    #[inline]
    fn add_state(&mut self, idx: usize, bits: u8) {
        if let Some(slot) = self.state.get_mut(idx) {
            *slot |= bits;
        }
    }

    /// Rotates all SoA columns over inclusive range `[lo..=hi]` right by 1,
    /// moving row `hi` to index `lo` and shifting `[lo..hi]` to `[lo + 1..=hi]`.
    fn rotate_right_1(&mut self, lo: usize, hi: usize) {
        if lo >= hi || hi >= self.len() {
            return;
        }
        let end = hi + 1;
        if let Some(s) = self.words.get_mut(lo..end) {
            s.rotate_right(1);
        }
        if let Some(s) = self.gpr_defs.get_mut(lo..end) {
            s.rotate_right(1);
        }
        if let Some(s) = self.gpr_uses.get_mut(lo..end) {
            s.rotate_right(1);
        }
        if let Some(s) = self.fp_defs.get_mut(lo..end) {
            s.rotate_right(1);
        }
        if let Some(s) = self.fp_uses.get_mut(lo..end) {
            s.rotate_right(1);
        }
        if let Some(s) = self.flags_rw.get_mut(lo..end) {
            s.rotate_right(1);
        }
        if let Some(s) = self.mem.get_mut(lo..end) {
            s.rotate_right(1);
        }
        if let Some(s) = self.latency.get_mut(lo..end) {
            s.rotate_right(1);
        }
        if let Some(s) = self.pipe.get_mut(lo..end) {
            s.rotate_right(1);
        }
        if let Some(s) = self.kind.get_mut(lo..end) {
            s.rotate_right(1);
        }
        if let Some(s) = self.state.get_mut(lo..end) {
            s.rotate_right(1);
        }
    }

    fn compact_in_place(&mut self) -> usize {
        let n = self.len();
        let mut dst = 0usize;
        for src in 0..n {
            if self.is_deleted(src) {
                continue;
            }
            if dst != src {
                let w = at_u32(&self.words, src);
                let gd = at_u32(&self.gpr_defs, src);
                let gu = at_u32(&self.gpr_uses, src);
                let fd = at_u32(&self.fp_defs, src);
                let fu = at_u32(&self.fp_uses, src);
                let fl = at_u8(&self.flags_rw, src);
                let mm = self.mem_at(src);
                let lt = at_u8(&self.latency, src);
                let pp = self.pipe_at(src);
                let kd = self.kind_at(src);
                let st = at_u8(&self.state, src);
                set_at(&mut self.words, dst, w);
                set_at(&mut self.gpr_defs, dst, gd);
                set_at(&mut self.gpr_uses, dst, gu);
                set_at(&mut self.fp_defs, dst, fd);
                set_at(&mut self.fp_uses, dst, fu);
                set_at(&mut self.flags_rw, dst, fl);
                set_at(&mut self.mem, dst, mm);
                set_at(&mut self.latency, dst, lt);
                set_at(&mut self.pipe, dst, pp);
                set_at(&mut self.kind, dst, kd);
                set_at(&mut self.state, dst, st);
            }
            dst += 1;
        }
        self.words.truncate(dst);
        self.gpr_defs.truncate(dst);
        self.gpr_uses.truncate(dst);
        self.fp_defs.truncate(dst);
        self.fp_uses.truncate(dst);
        self.flags_rw.truncate(dst);
        self.mem.truncate(dst);
        self.latency.truncate(dst);
        self.pipe.truncate(dst);
        self.kind.truncate(dst);
        self.state.truncate(dst);
        dst
    }
}

/// Mutable optimizer state wrapping `MachFuncSoa` plus remapped metadata.
struct MachContext<'a> {
    soa: MachFuncSoa<'a>,
    block_word_off: Vec<usize, Alloc<'a>>,
    block_order: Vec<usize, Alloc<'a>>,
    patch_word_idxs: Vec<usize, Alloc<'a>>,
    patch_target_blocks: Vec<u32, Alloc<'a>>,
    reloc_byte_offs: Vec<u32, Alloc<'a>>,
    trap_byte_offs: Vec<u32, Alloc<'a>>,
    extra_word_idxs: Vec<usize, Alloc<'a>>,
    pinned_targets: Vec<usize, Alloc<'a>>,
    internal_branches: Vec<InternalBranch, Alloc<'a>>,
    /// Exact bitmask of GPRs live-in at `ret` (always includes [`RET_CALLEE_SAVED_GPRS`]).
    ret_live_gprs: u32,
    /// False if the function contains a non-reloc PC-relative `adr` / `ldr_lit` (e.g. jump table),
    /// in which case no words are deleted so data-table byte offsets remain bit-exact.
    allow_delete: bool,
    /// True if any instruction was marked deleted since the last compaction.
    dirty_deletes: bool,
    alloc: Alloc<'a>,
}

impl<'a> MachContext<'a> {
    fn build(input: &MachOptimizeInput<'a, '_>, alloc: Alloc<'a>) -> Self {
        let n = input.words.len();
        let ret_live_gprs = if input.ret_live_gprs == 0 {
            RET_LIVE_GPRS
        } else {
            input.ret_live_gprs | RET_CALLEE_SAVED_GPRS
        };
        let mut soa = MachFuncSoa::from_words(input.words, ret_live_gprs, alloc);

        let mut block_word_off = Vec::with_capacity_in(input.block_word_off.len(), alloc);
        block_word_off.extend_from_slice(input.block_word_off);

        let mut block_order = Vec::with_capacity_in(input.block_order.len(), alloc);
        block_order.extend_from_slice(input.block_order);

        let mut patch_word_idxs = Vec::with_capacity_in(input.patch_word_idxs.len(), alloc);
        patch_word_idxs.extend_from_slice(input.patch_word_idxs);

        let mut patch_target_blocks = Vec::with_capacity_in(input.patch_target_blocks.len(), alloc);
        patch_target_blocks.extend_from_slice(input.patch_target_blocks);

        let mut reloc_byte_offs = Vec::with_capacity_in(input.reloc_byte_offs.len(), alloc);
        reloc_byte_offs.extend_from_slice(input.reloc_byte_offs);

        let mut trap_byte_offs = Vec::with_capacity_in(input.trap_byte_offs.len(), alloc);
        trap_byte_offs.extend_from_slice(input.trap_byte_offs);

        let mut extra_word_idxs = Vec::with_capacity_in(input.extra_word_idxs.len(), alloc);
        extra_word_idxs.extend_from_slice(input.extra_word_idxs);

        let mut pinned_targets = Vec::with_capacity_in(input.cold_stub_word_idxs.len(), alloc);
        pinned_targets.extend_from_slice(input.cold_stub_word_idxs);

        // Mark block starts & cold stubs as target boundaries.
        for &off in &block_word_off {
            if off < n {
                soa.add_state(off, STATE_TARGET_BOUNDARY);
            }
        }
        for &off in &pinned_targets {
            if off < n {
                soa.add_state(off, STATE_TARGET_BOUNDARY);
            }
        }

        // Mark patches, relocs, traps, and caller-tracked extra sites as pinned.
        let mut is_patch = vec_filled_in(n, false, alloc);
        for &pidx in &patch_word_idxs {
            if pidx < n {
                set_at(&mut is_patch, pidx, true);
                soa.add_state(pidx, STATE_PINNED);
            }
        }
        let mut is_reloc = vec_filled_in(n, false, alloc);
        for &byte_off in &reloc_byte_offs {
            let widx = to_idx(byte_off) / 4;
            if widx < n {
                set_at(&mut is_reloc, widx, true);
                soa.add_state(widx, STATE_PINNED | STATE_RELOC_OR_TRAP);
            }
        }
        for &byte_off in &trap_byte_offs {
            let widx = to_idx(byte_off) / 4;
            if widx < n {
                soa.add_state(widx, STATE_PINNED | STATE_RELOC_OR_TRAP);
            }
        }
        let mut is_extra_pinned = vec_filled_in(n, false, alloc);
        for &eidx in &extra_word_idxs {
            if eidx < n {
                set_at(&mut is_extra_pinned, eidx, true);
                soa.add_state(eidx, STATE_PINNED);
            }
        }
        for &vidx in input.volatile_word_idxs {
            if vidx < n {
                soa.add_state(vidx, STATE_PINNED);
                set_at(&mut soa.mem, vidx, MemEffect::Barrier);
            }
        }

        // Scan pre-resolved internal PC-relative branches & detect local `adr`/`ldr_lit` jump tables.
        let mut internal_branches: Vec<InternalBranch, Alloc<'a>> = Vec::with_capacity_in(16, alloc);
        let mut allow_delete = true;
        for widx in 0..n {
            let w = at_u32(&soa.words, widx);
            let patched = is_patch.get(widx).copied().unwrap_or(false);
            let reloced = is_reloc.get(widx).copied().unwrap_or(false);
            let extra_pinned = is_extra_pinned.get(widx).copied().unwrap_or(false);

            // Detect non-reloc local `adr` (`op=0`), `adrp` (`op=1`), or `ldr (literal)` (`0x5800_0000` / `0x1800_0000`).
            // Skip `extra_pinned` sites (e.g. unresolved `shared_jump_tables` placeholder `adr(X17, 0)`),
            // which the caller resolves using the remapped `extra_word_idxs` AFTER `optimize_mach_a64`.
            if !reloced
                && !extra_pinned
                && ((w & 0x1f00_0000) == 0x1000_0000 || (w & 0x3b00_0000) == 0x1800_0000)
            {
                allow_delete = false;
                soa.add_state(widx, STATE_PINNED);
                continue;
            }

            if patched || reloced || extra_pinned {
                continue;
            }

            let branch_info = if (w & 0xfc00_0000) == 0x1400_0000 {
                Some((sext_bits(w & 0x3ff_ffff, 26), Field::Imm26))
            } else if (w & 0xfc00_0000) == 0x9400_0000 {
                // Local PC-relative `bl` (e.g. SME2 micro-kernel stub at tail).
                Some((sext_bits(w & 0x3ff_ffff, 26), Field::Imm26))
            } else if (w & 0xff00_0010) == 0x5400_0000 || (w & 0x7e00_0000) == 0x3400_0000 {
                Some((sext_bits((w >> 5) & 0x7_ffff, 19), Field::Imm19))
            } else if (w & 0x7e00_0000) == 0x3600_0000 {
                Some((sext_bits((w >> 5) & 0x3fff, 14), Field::Imm14))
            } else {
                None
            };

            if let Some((delta_words, field)) = branch_info
                && let Some(src_i64) = i64_of(widx)
            {
                let dst_i64 = src_i64 + i64::from(delta_words);
                if let Ok(dst_word) = usize::try_from(dst_i64)
                    && dst_word <= n
                {
                    if dst_word < n {
                        soa.add_state(dst_word, STATE_TARGET_BOUNDARY);
                    }
                    internal_branches.push(InternalBranch { src_word: widx, dst_word, field });
                }
            }
        }

        // If the function has inline jump-table `adr`/`br` sequences, mark words strictly after
        // an indirect `br` up to the next target boundary as opaque data so no pass reorders them.
        if !allow_delete {
            let mut in_opaque = false;
            for widx in 0..n {
                if soa.is_target_boundary(widx) {
                    in_opaque = false;
                }
                if in_opaque {
                    soa.add_state(widx, STATE_OPAQUE_DATA | STATE_PINNED);
                } else if matches!(soa.kind_at(widx), DecodedKind::IndirectBranch) {
                    in_opaque = true;
                }
            }
        }

        Self {
            soa,
            block_word_off,
            block_order,
            patch_word_idxs,
            patch_target_blocks,
            reloc_byte_offs,
            trap_byte_offs,
            extra_word_idxs,
            pinned_targets,
            internal_branches,
            ret_live_gprs,
            allow_delete,
            dirty_deletes: false,
            alloc,
        }
    }

    #[inline]
    fn mark_deleted(&mut self, idx: usize) {
        if self.allow_delete && idx < self.soa.len() && !self.soa.is_deleted(idx) {
            self.soa.add_state(idx, STATE_DELETED);
            self.dirty_deletes = true;
        }
    }

    /// Rebuilds the dense SoA table after any deletions and updates EVERY offset table
    /// (`block_word_off`, `patch_word_idxs`, `reloc_byte_offs`, `trap_byte_offs`,
    /// `extra_word_idxs`, `pinned_targets`, and pre-resolved `internal_branches`) in a single O(N) pass.
    fn compact_and_remap(&mut self) {
        if !self.dirty_deletes {
            return;
        }
        let old_len = self.soa.len();
        let mut old_to_new: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(old_len + 1, self.alloc);
        let mut was_deleted: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(old_len, self.alloc);
        let mut dst = 0usize;
        for i in 0..old_len {
            let del = self.soa.is_deleted(i);
            was_deleted.push(del);
            old_to_new.push(dst);
            if !del {
                dst += 1;
            }
        }
        old_to_new.push(dst);

        let new_len = self.soa.compact_in_place();
        debug_assert_eq!(new_len, dst);

        // Remap block_word_off.
        for slot in self.block_word_off.iter_mut() {
            if *slot <= old_len {
                *slot = at_usize(&old_to_new, *slot);
            }
        }

        // Remap patch_word_idxs (elided patches become `ELIDED_PATCH`).
        for slot in self.patch_word_idxs.iter_mut() {
            if *slot != ELIDED_PATCH && *slot < old_len {
                if was_deleted.get(*slot).copied().unwrap_or(false) {
                    *slot = ELIDED_PATCH;
                } else {
                    *slot = at_usize(&old_to_new, *slot);
                }
            }
        }

        // Remap reloc_byte_offs & trap_byte_offs.
        for slot in self.reloc_byte_offs.iter_mut() {
            let old_w = to_idx(*slot) / 4;
            let rem = *slot % 4;
            if old_w <= old_len {
                *slot = u32_of(at_usize(&old_to_new, old_w) * 4) + rem;
            }
        }
        for slot in self.trap_byte_offs.iter_mut() {
            let old_w = to_idx(*slot) / 4;
            let rem = *slot % 4;
            if old_w <= old_len {
                *slot = u32_of(at_usize(&old_to_new, old_w) * 4) + rem;
            }
        }

        // Remap caller-tracked extra pinned word indices.
        for slot in self.extra_word_idxs.iter_mut() {
            if *slot <= old_len {
                *slot = at_usize(&old_to_new, *slot);
            }
        }

        // Remap pinned_targets and re-assert boundary flags on the compacted table.
        for slot in self.pinned_targets.iter_mut() {
            if *slot <= old_len {
                *slot = at_usize(&old_to_new, *slot);
            }
        }

        // Remap & re-patch all pre-resolved internal PC-relative branches.
        let mut live_br = 0usize;
        for i in 0..self.internal_branches.len() {
            let Some(&br) = self.internal_branches.get(i) else {
                continue;
            };
            if br.src_word >= old_len || was_deleted.get(br.src_word).copied().unwrap_or(false) {
                continue;
            }
            let new_src = at_usize(&old_to_new, br.src_word);
            let new_dst = at_usize(&old_to_new, br.dst_word);
            if let (Some(src_i), Some(dst_i)) = (i64_of(new_src), i64_of(new_dst)) {
                let bytes = (dst_i - src_i) * 4;
                let old_word = at_u32(&self.soa.words, new_src);
                if let Some(patched_word) = br.field.patch(old_word, bytes) {
                    self.soa.replace_word(new_src, patched_word);
                }
            }
            set_at(
                &mut self.internal_branches,
                live_br,
                InternalBranch { src_word: new_src, dst_word: new_dst, field: br.field },
            );
            live_br += 1;
        }
        self.internal_branches.truncate(live_br);

        // Re-assert `STATE_TARGET_BOUNDARY` on block starts, pinned targets, and branch destinations
        // (necessary if the first instruction of a block was deleted and the boundary shifted to the next word).
        for &off in &self.block_word_off {
            if off < new_len {
                self.soa.add_state(off, STATE_TARGET_BOUNDARY);
            }
        }
        for &off in &self.pinned_targets {
            if off < new_len {
                self.soa.add_state(off, STATE_TARGET_BOUNDARY);
            }
        }
        for br in &self.internal_branches {
            if br.dst_word < new_len {
                self.soa.add_state(br.dst_word, STATE_TARGET_BOUNDARY);
            }
        }

        self.dirty_deletes = false;
    }

    /// Final pass right before `write_back`: inserts minimal `nop` padding before every block `b`
    /// with `align16_blocks[b] == true` so `block_word_off[b] % 4 == 0` in the emitted code,
    /// and remaps every offset table and pre-resolved internal branch in lockstep.
    ///
    /// Aborts cleanly (leaving `self` untouched) if inserting padding would overflow any
    /// pre-resolved internal branch immediate range.
    fn realign_loop_headers(&mut self, align16_blocks: &[bool]) {
        if !self.allow_delete || align16_blocks.is_empty() || self.dirty_deletes {
            return;
        }
        let old_len = self.soa.len();
        let mut need_align_at = vec_filled_in(old_len + 1, false, self.alloc);
        let mut align_count = 0usize;
        for &b in &self.block_order {
            if align16_blocks.get(b).copied().unwrap_or(false) {
                let off = at_usize(&self.block_word_off, b);
                if off > 0 && off < old_len && !need_align_at.get(off).copied().unwrap_or(false) {
                    set_at(&mut need_align_at, off, true);
                    align_count += 1;
                }
            }
        }
        if align_count == 0 {
            return;
        }

        let cap = old_len.saturating_add(align_count.saturating_mul(3));
        let mut new_words: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(cap, self.alloc);
        let mut old_to_new: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(old_len + 1, self.alloc);
        for i in 0..old_len {
            if need_align_at.get(i).copied().unwrap_or(false) {
                while (new_words.len() & 3) != 0 {
                    new_words.push(enc::nop());
                }
            }
            old_to_new.push(new_words.len());
            new_words.push(at_u32(&self.soa.words, i));
        }
        old_to_new.push(new_words.len());

        if new_words.len() == old_len {
            return;
        }

        // Pre-verify & re-patch all pre-resolved internal branches on `new_words` before committing.
        // If any branch overflows its field (e.g. `Imm14` in a giant function), abort realignment
        // leaving the unpadded code and all metadata completely unchanged.
        for br in &self.internal_branches {
            if br.src_word >= old_len || br.dst_word > old_len {
                return;
            }
            let new_src = at_usize(&old_to_new, br.src_word);
            let new_dst = at_usize(&old_to_new, br.dst_word);
            let (Some(src_i), Some(dst_i)) = (i64_of(new_src), i64_of(new_dst)) else {
                return;
            };
            let bytes = (dst_i - src_i) * 4;
            let old_word = at_u32(&new_words, new_src);
            let Some(patched_word) = br.field.patch(old_word, bytes) else {
                return;
            };
            set_at(&mut new_words, new_src, patched_word);
        }

        // Commit padded words and remap every offset table in lockstep.
        self.soa.words = new_words;

        for slot in self.block_word_off.iter_mut() {
            if *slot <= old_len {
                *slot = at_usize(&old_to_new, *slot);
            }
        }
        for slot in self.patch_word_idxs.iter_mut() {
            if *slot != ELIDED_PATCH && *slot < old_len {
                *slot = at_usize(&old_to_new, *slot);
            }
        }
        for slot in self.reloc_byte_offs.iter_mut() {
            let old_w = to_idx(*slot) / 4;
            let rem = *slot % 4;
            if old_w <= old_len {
                *slot = u32_of(at_usize(&old_to_new, old_w) * 4) + rem;
            }
        }
        for slot in self.trap_byte_offs.iter_mut() {
            let old_w = to_idx(*slot) / 4;
            let rem = *slot % 4;
            if old_w <= old_len {
                *slot = u32_of(at_usize(&old_to_new, old_w) * 4) + rem;
            }
        }
        for slot in self.extra_word_idxs.iter_mut() {
            if *slot <= old_len {
                *slot = at_usize(&old_to_new, *slot);
            }
        }
        for slot in self.pinned_targets.iter_mut() {
            if *slot <= old_len {
                *slot = at_usize(&old_to_new, *slot);
            }
        }
        for br in self.internal_branches.iter_mut() {
            br.src_word = at_usize(&old_to_new, br.src_word);
            br.dst_word = at_usize(&old_to_new, br.dst_word);
        }
    }

    fn write_back(&self, input: &mut MachOptimizeInput<'a, '_>) {
        input.words.clear();
        input.words.extend_from_slice(&self.soa.words);
        for (dst, &src) in input.block_word_off.iter_mut().zip(self.block_word_off.iter()) {
            *dst = src;
        }
        for (dst, &src) in input.patch_word_idxs.iter_mut().zip(self.patch_word_idxs.iter()) {
            *dst = src;
        }
        for (dst, &src) in input.reloc_byte_offs.iter_mut().zip(self.reloc_byte_offs.iter()) {
            *dst = src;
        }
        for (dst, &src) in input.trap_byte_offs.iter_mut().zip(self.trap_byte_offs.iter()) {
            *dst = src;
        }
        for (dst, &src) in input.extra_word_idxs.iter_mut().zip(self.extra_word_idxs.iter()) {
            *dst = src;
        }
    }
}

// =============================================================================
// 4. Exact Machine-Basic-Block CFG & Cross-Block `u32` GPR Liveness (Pass 1b)
// =============================================================================

/// Structure-of-Arrays Machine Basic Block CFG and exact backward GPR liveness.
struct MachCfgLiveness<'a> {
    mbb_start: Vec<usize, Alloc<'a>>,
    mbb_end: Vec<usize, Alloc<'a>>,
    succ0: Vec<u32, Alloc<'a>>,
    succ1: Vec<u32, Alloc<'a>>,
    unknown_succ: Vec<bool, Alloc<'a>>,
    pred_count: Vec<u32, Alloc<'a>>,
    pred_off: Vec<u32, Alloc<'a>>,
    pred_list: Vec<u32, Alloc<'a>>,
    use_mask: Vec<u32, Alloc<'a>>,
    def_mask: Vec<u32, Alloc<'a>>,
    live_in: Vec<u32, Alloc<'a>>,
    live_out: Vec<u32, Alloc<'a>>,
    /// Bitmask of all GPRs written anywhere in the function (gates callee-saved scratch reuse).
    func_written_gprs: u32,
    /// True if any indirect branch (`br`) or unresolved patch prevents proving predecessor completeness.
    has_indirect_or_unknown: bool,
}

impl<'a> MachCfgLiveness<'a> {
    fn num_mbbs(&self) -> usize {
        self.mbb_start.len()
    }
}

/// Builds exact straight-line Machine Basic Blocks (splitting at every block start, internal
/// branch target, cold stub, and after every branch/terminator) and computes exact cross-block
/// `live_in[m]: u32` / `live_out[m]: u32` GPR bitmasks via bounded backward fixpoint iteration.
fn compute_live_out_gprs<'a>(ctx: &MachContext<'a>) -> MachCfgLiveness<'a> {
    let alloc = ctx.alloc;
    let n = ctx.soa.len();
    let mut is_mbb_start = vec_filled_in(n, false, alloc);
    if n > 0 {
        set_at(&mut is_mbb_start, 0, true);
    }

    let mut func_written_gprs = 0u32;
    let mut has_indirect_or_unknown = false;

    for i in 0..n {
        if ctx.soa.is_target_boundary(i) {
            set_at(&mut is_mbb_start, i, true);
        }
        let kd = ctx.soa.kind_at(i);
        if i + 1 < n
            && (matches!(
                kd,
                DecodedKind::BranchUncond { .. }
                    | DecodedKind::BranchCond { .. }
                    | DecodedKind::Ret
                    | DecodedKind::IndirectBranch
            ) || enc::is_unconditional_terminator(at_u32(&ctx.soa.words, i)))
        {
            set_at(&mut is_mbb_start, i + 1, true);
        }
        if matches!(kd, DecodedKind::IndirectBranch) {
            has_indirect_or_unknown = true;
        }
        if !ctx.soa.is_opaque_data(i) && ctx.soa.gpr_defs.get(i).copied().unwrap_or(0) != u32::MAX {
            func_written_gprs |= at_u32(&ctx.soa.gpr_defs, i);
        }
    }

    // Build MBB ranges and `word_to_mbb` lookup table.
    let mut mbb_start: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(ctx.block_order.len() + 8, alloc);
    let mut mbb_end: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(ctx.block_order.len() + 8, alloc);
    let mut word_to_mbb = vec_filled_in(n + 1, NO_MBB, alloc);

    let mut cur = 0usize;
    while cur < n {
        let m_id = u32_of(mbb_start.len());
        let start = cur;
        cur += 1;
        while cur < n && !is_mbb_start.get(cur).copied().unwrap_or(false) {
            cur += 1;
        }
        mbb_start.push(start);
        mbb_end.push(cur);
        for w in start..cur {
            set_at(&mut word_to_mbb, w, m_id);
        }
    }

    let num_mbbs = mbb_start.len();
    let mut succ0 = vec_filled_in(num_mbbs, NO_MBB, alloc);
    let mut succ1 = vec_filled_in(num_mbbs, NO_MBB, alloc);
    let mut unknown_succ = vec_filled_in(num_mbbs, false, alloc);
    let mut pred_count = vec_filled_in(num_mbbs, 0u32, alloc);

    // Map patch word index -> patch array index so we can look up `patch_target_blocks`.
    let has_patch_targets = ctx.patch_target_blocks.len() == ctx.patch_word_idxs.len();
    let mut word_patch_target = vec_filled_in(n, NO_MBB, alloc);
    let mut word_is_unmapped_patch = vec_filled_in(n, false, alloc);
    for (p, &widx) in ctx.patch_word_idxs.iter().enumerate() {
        if widx == ELIDED_PATCH || widx >= n {
            continue;
        }
        if has_patch_targets {
            let tb = to_idx(at_u32(&ctx.patch_target_blocks, p));
            if let Some(&target_w) = ctx.block_word_off.get(tb)
                && target_w < n
            {
                let target_mbb = at_u32(&word_to_mbb, target_w);
                set_at(&mut word_patch_target, widx, target_mbb);
            } else {
                set_at(&mut word_is_unmapped_patch, widx, true);
                has_indirect_or_unknown = true;
            }
        } else {
            set_at(&mut word_is_unmapped_patch, widx, true);
            has_indirect_or_unknown = true;
        }
    }

    // Wire CFG edges for every MBB.
    for m in 0..num_mbbs {
        let start = at_usize(&mbb_start, m);
        let end = at_usize(&mbb_end, m);
        if start >= end {
            continue;
        }
        let last = end - 1;
        let last_word = at_u32(&ctx.soa.words, last);
        let last_kind = ctx.soa.kind_at(last);
        let mut s0 = NO_MBB;
        let mut s1 = NO_MBB;
        let mut unk = ctx.soa.is_opaque_data(last);

        let resolve_target = |delta_words: i32| -> Option<u32> {
            if at_u32(&word_patch_target, last) != NO_MBB {
                return Some(at_u32(&word_patch_target, last));
            }
            if word_is_unmapped_patch.get(last).copied().unwrap_or(false)
                || ctx.soa.is_reloc_or_trap(last)
            {
                return None;
            }
            let src_i = i64_of(last)?;
            let dst_i = src_i + i64::from(delta_words);
            let dst_w = usize::try_from(dst_i).ok()?;
            if dst_w < n {
                let tm = at_u32(&word_to_mbb, dst_w);
                if tm != NO_MBB { Some(tm) } else { None }
            } else {
                None
            }
        };

        match last_kind {
            DecodedKind::BranchUncond { delta_words } => {
                if ctx.soa.is_reloc_or_trap(last) {
                    // Direct tail call (`b sym` with Branch26 reloc): exits function cleanly, uses all arg/callee-saved regs.
                    unk = true;
                } else if let Some(t) = resolve_target(delta_words) {
                    s0 = t;
                } else {
                    unk = true;
                }
            }
            DecodedKind::BranchCond { delta_words, .. } => {
                if let Some(t) = resolve_target(delta_words) {
                    s0 = t;
                } else {
                    unk = true;
                }
                if m + 1 < num_mbbs {
                    s1 = u32_of(m + 1);
                } else {
                    unk = true;
                }
            }
            DecodedKind::Ret => {
                // Exits function (`Ret` already lists `ctx.ret_live_gprs` in `gpr_uses`).
            }
            DecodedKind::IndirectBranch => {
                unk = true;
            }
            _ => {
                if !enc::is_unconditional_terminator(last_word) {
                    if m + 1 < num_mbbs {
                        s0 = u32_of(m + 1);
                    } else {
                        unk = true;
                    }
                }
            }
        }

        if s0 == s1 {
            s1 = NO_MBB;
        }
        set_at(&mut succ0, m, s0);
        set_at(&mut succ1, m, s1);
        set_at(&mut unknown_succ, m, unk);

        for s in [s0, s1] {
            if s != NO_MBB {
                let si = to_idx(s);
                let prev_c = at_u32(&pred_count, si);
                set_at(&mut pred_count, si, prev_c.saturating_add(1));
            }
        }
    }

    // Build compact CSR predecessor table (`pred_off`, `pred_list`) in O(V + E).
    let mut pred_off = vec_filled_in(num_mbbs + 1, 0u32, alloc);
    let mut total_preds = 0u32;
    for m in 0..num_mbbs {
        set_at(&mut pred_off, m, total_preds);
        total_preds = total_preds.saturating_add(at_u32(&pred_count, m));
    }
    set_at(&mut pred_off, num_mbbs, total_preds);

    let mut pred_cursor = Vec::with_capacity_in(num_mbbs, alloc);
    for m in 0..num_mbbs {
        pred_cursor.push(at_u32(&pred_off, m));
    }
    let mut pred_list = vec_filled_in(to_idx(total_preds), NO_MBB, alloc);
    for m in 0..num_mbbs {
        let s0 = at_u32(&succ0, m);
        let s1 = at_u32(&succ1, m);
        for s in [s0, s1] {
            if s != NO_MBB {
                let si = to_idx(s);
                let pos = to_idx(at_u32(&pred_cursor, si));
                set_at(&mut pred_list, pos, u32_of(m));
                set_at(&mut pred_cursor, si, u32_of(pos.saturating_add(1)));
            }
        }
    }

    // Compute per-MBB local `use_mask` and `def_mask`.
    let mut use_mask = vec_filled_in(num_mbbs, 0u32, alloc);
    let mut def_mask = vec_filled_in(num_mbbs, 0u32, alloc);
    for m in 0..num_mbbs {
        let start = at_usize(&mbb_start, m);
        let end = at_usize(&mbb_end, m);
        let mut uses = 0u32;
        let mut defs = 0u32;
        for i in start..end {
            if ctx.soa.is_deleted(i) {
                continue;
            }
            uses |= at_u32(&ctx.soa.gpr_uses, i) & !defs;
            defs |= at_u32(&ctx.soa.gpr_defs, i);
        }
        set_at(&mut use_mask, m, uses);
        set_at(&mut def_mask, m, defs);
    }

    // Backward bitmask fixpoint over MBBs (32 bits per MBB, converges in ~3 rounds).
    let mut live_in = vec_filled_in(num_mbbs, 0u32, alloc);
    let mut live_out = vec_filled_in(num_mbbs, 0u32, alloc);
    for m in 0..num_mbbs {
        set_at(&mut live_in, m, at_u32(&use_mask, m));
        if unknown_succ.get(m).copied().unwrap_or(false) {
            set_at(&mut live_out, m, u32::MAX);
        }
    }

    let mut converged = false;
    for _ in 0..MAX_LIVENESS_ITERS {
        let mut changed = false;
        for m in (0..num_mbbs).rev() {
            let mut out = if unknown_succ.get(m).copied().unwrap_or(false) {
                u32::MAX
            } else {
                0
            };
            let s0 = at_u32(&succ0, m);
            if s0 != NO_MBB {
                out |= at_u32(&live_in, to_idx(s0));
            }
            let s1 = at_u32(&succ1, m);
            if s1 != NO_MBB {
                out |= at_u32(&live_in, to_idx(s1));
            }
            let new_in = at_u32(&use_mask, m) | (out & !at_u32(&def_mask, m));
            if out != at_u32(&live_out, m) || new_in != at_u32(&live_in, m) {
                set_at(&mut live_out, m, out);
                set_at(&mut live_in, m, new_in);
                changed = true;
            }
        }
        if !changed {
            converged = true;
            break;
        }
    }

    if !converged {
        for m in 0..num_mbbs {
            set_at(&mut live_in, m, u32::MAX);
            set_at(&mut live_out, m, u32::MAX);
        }
    }

    MachCfgLiveness {
        mbb_start,
        mbb_end,
        succ0,
        succ1,
        unknown_succ,
        pred_count,
        pred_off,
        pred_list,
        use_mask,
        def_mask,
        live_in,
        live_out,
        func_written_gprs,
        has_indirect_or_unknown,
    }
}

/// Computes `live_after[idx - start]` for every instruction `idx` in `[start..end)` of an MBB.
fn compute_mbb_live_after<'a>(
    soa: &MachFuncSoa<'a>,
    start: usize,
    end: usize,
    live_out: u32,
    out_buf: &mut Vec<u32, Alloc<'a>>,
) {
    out_buf.clear();
    let len = end.saturating_sub(start);
    out_buf.reserve(len);
    for _ in 0..len {
        out_buf.push(0);
    }
    let mut live = live_out;
    for idx in (start..end).rev() {
        set_at(out_buf, idx - start, live);
        if !soa.is_deleted(idx) {
            live = (live & !at_u32(&soa.gpr_defs, idx)) | at_u32(&soa.gpr_uses, idx);
        }
    }
}

// =============================================================================
// 5. Verified Sound GPR Operand Rewriting Helpers (for Pass 1)
// =============================================================================

#[inline]
fn set_field_5(w: u32, shift: u32, reg: u8) -> u32 {
    (w & !(31u32 << shift)) | (u32::from(reg & 31) << shift)
}

/// True iff `w` is an instruction whose GPR destination field (bits `4:0`) can be cleanly
/// retargeted to any `new_rd < 31` without altering implicit inputs or `SP`/`XZR` semantics.
fn can_rewrite_gpr_def(soa: &MachFuncSoa<'_>, idx: usize, old_rd: u8) -> bool {
    if old_rd >= 31 || soa.is_deleted(idx) || soa.is_pinned(idx) || soa.is_opaque_data(idx) {
        return false;
    }
    let w = at_u32(&soa.words, idx);
    match soa.kind_at(idx) {
        DecodedKind::MovReg { rd, .. }
        | DecodedKind::Csel { rd, .. }
        | DecodedKind::Cset { rd, .. }
        | DecodedKind::AddSubImm { rd, .. }
        | DecodedKind::AddSubReg { rd, .. } => rd == old_rd,
        DecodedKind::LdrOff { rt, is_fp, .. } => !is_fp && rt == old_rd,
        DecodedKind::Other => {
            if reg5(w) != old_rd {
                return false;
            }
            // Pure single-GPR-def instructions that do NOT implicitly read Rd (`movk`/`bfm` excluded!).
            let is_csel_family = (w & 0x3fe0_0800) == 0x1a80_0000 && ((w >> 10) & 3) <= 1;
            let is_log_reg = (w & 0x1f00_0000) == 0x0a00_0000;
            let is_log_imm = (w & 0x1f80_0000) == 0x1200_0000;
            let is_addsub_shifted = (w & 0x1f20_0000) == 0x0b00_0000 && ((w >> 22) & 3) != 3;
            let is_movz_movn = (w & 0x1f80_0000) == 0x1280_0000 && (((w >> 29) & 3) == 0 || ((w >> 29) & 3) == 2);
            let is_sbfm_ubfm = (w & 0x1f80_0000) == 0x1300_0000 && (((w >> 29) & 3) == 0 || ((w >> 29) & 3) == 2);
            let is_madd_msub = (w & 0x7fe0_0000) == 0x1b00_0000;
            is_csel_family
                || is_log_reg
                || is_log_imm
                || is_addsub_shifted
                || is_movz_movn
                || is_sbfm_ubfm
                || is_madd_msub
        }
        _ => false,
    }
}

/// Rewrites every GPR use (and optionally GPR def) of `old_r` (`< 31`) in instruction `idx` to
/// `new_r` (`< 31`), returning `Some(new_word)` iff the instruction's exact bitfields are verified.
fn rewrite_gpr_in_inst(
    soa: &MachFuncSoa<'_>,
    idx: usize,
    old_r: u8,
    new_r: u8,
    rewrite_uses: bool,
    rewrite_def: bool,
) -> Option<u32> {
    if old_r >= 31 || new_r >= 31 || soa.is_deleted(idx) || soa.is_pinned(idx) || soa.is_opaque_data(idx) {
        return None;
    }
    let w = at_u32(&soa.words, idx);
    let mut nw = w;
    let old_bit = 1u32 << old_r;
    let has_use = (at_u32(&soa.gpr_uses, idx) & old_bit) != 0;
    let has_def = (at_u32(&soa.gpr_defs, idx) & old_bit) != 0;
    if (!rewrite_uses || !has_use) && (!rewrite_def || !has_def) {
        return Some(w);
    }

    match soa.kind_at(idx) {
        DecodedKind::MovReg { rd, rn, .. } => {
            if rewrite_def && rd == old_r {
                nw = set_field_5(nw, 0, new_r);
            }
            if rewrite_uses && rn == old_r {
                nw = set_field_5(nw, 16, new_r);
            }
            Some(nw)
        }
        DecodedKind::Csel { rd, rn, rm, .. } => {
            if rewrite_def && rd == old_r {
                nw = set_field_5(nw, 0, new_r);
            }
            if rewrite_uses {
                if rn == old_r {
                    nw = set_field_5(nw, 5, new_r);
                }
                if rm == old_r {
                    nw = set_field_5(nw, 16, new_r);
                }
            }
            Some(nw)
        }
        DecodedKind::Cset { rd, .. } => {
            if rewrite_def && rd == old_r {
                nw = set_field_5(nw, 0, new_r);
            }
            Some(nw)
        }
        DecodedKind::CmpZero { rn, .. } => {
            if rewrite_uses && rn == old_r {
                nw = set_field_5(nw, 5, new_r);
            }
            Some(nw)
        }
        DecodedKind::AddSubImm { rd, rn, .. } => {
            if rewrite_def && rd == old_r {
                nw = set_field_5(nw, 0, new_r);
            }
            if rewrite_uses && rn == old_r {
                nw = set_field_5(nw, 5, new_r);
            }
            Some(nw)
        }
        DecodedKind::AddSubReg { rd, rn, rm, .. } => {
            if rewrite_def && rd == old_r {
                nw = set_field_5(nw, 0, new_r);
            }
            if rewrite_uses {
                if rn == old_r {
                    nw = set_field_5(nw, 5, new_r);
                }
                if rm == old_r {
                    nw = set_field_5(nw, 16, new_r);
                }
            }
            Some(nw)
        }
        DecodedKind::LdrOff { rt, rn, is_fp, .. } => {
            if rewrite_def && !is_fp && rt == old_r {
                nw = set_field_5(nw, 0, new_r);
            }
            if rewrite_uses && rn == old_r {
                nw = set_field_5(nw, 5, new_r);
            }
            Some(nw)
        }
        DecodedKind::StrOff { rt, rn, is_fp, .. } => {
            if rewrite_uses {
                if !is_fp && rt == old_r {
                    nw = set_field_5(nw, 0, new_r);
                }
                if rn == old_r {
                    nw = set_field_5(nw, 5, new_r);
                }
            }
            Some(nw)
        }
        DecodedKind::Other => {
            // Explicitly verified `csinc`/`csinv`/`csneg`, logical shifted reg, add/sub shifted reg,
            // logical imm, `sbfm`/`ubfm`, `madd`/`msub`, `ccmp`.
            let is_csel_family = (w & 0x3fe0_0800) == 0x1a80_0000 && ((w >> 10) & 3) <= 1;
            let is_log_reg = (w & 0x1f00_0000) == 0x0a00_0000;
            let is_addsub_shifted = (w & 0x1f20_0000) == 0x0b00_0000 && ((w >> 22) & 3) != 3;
            if is_csel_family || is_log_reg || is_addsub_shifted {
                if rewrite_def && reg5(w) == old_r {
                    nw = set_field_5(nw, 0, new_r);
                }
                if rewrite_uses {
                    if reg5(w >> 5) == old_r {
                        nw = set_field_5(nw, 5, new_r);
                    }
                    if reg5(w >> 16) == old_r {
                        nw = set_field_5(nw, 16, new_r);
                    }
                }
                return Some(nw);
            }
            let is_log_imm = (w & 0x1f80_0000) == 0x1200_0000;
            let is_sbfm_ubfm = (w & 0x1f80_0000) == 0x1300_0000 && (((w >> 29) & 3) == 0 || ((w >> 29) & 3) == 2);
            if is_log_imm || is_sbfm_ubfm {
                if rewrite_def && reg5(w) == old_r {
                    nw = set_field_5(nw, 0, new_r);
                }
                if rewrite_uses && reg5(w >> 5) == old_r {
                    nw = set_field_5(nw, 5, new_r);
                }
                return Some(nw);
            }
            let is_madd_msub = (w & 0x7fe0_0000) == 0x1b00_0000;
            if is_madd_msub {
                if rewrite_def && reg5(w) == old_r {
                    nw = set_field_5(nw, 0, new_r);
                }
                if rewrite_uses {
                    if reg5(w >> 5) == old_r {
                        nw = set_field_5(nw, 5, new_r);
                    }
                    if reg5(w >> 16) == old_r {
                        nw = set_field_5(nw, 16, new_r);
                    }
                    if reg5(w >> 10) == old_r {
                        nw = set_field_5(nw, 10, new_r);
                    }
                }
                return Some(nw);
            }
            let is_ccmp = (w & 0x3fe0_0410) == 0x3a40_0000;
            if is_ccmp && !rewrite_def {
                if rewrite_uses {
                    if reg5(w >> 5) == old_r {
                        nw = set_field_5(nw, 5, new_r);
                    }
                    if ((w >> 11) & 1) == 0 && reg5(w >> 16) == old_r {
                        nw = set_field_5(nw, 16, new_r);
                    }
                }
                return Some(nw);
            }
            None
        }
        _ => None,
    }
}

// =============================================================================
// 6. Pass 1 — `mach_peephole_a64`
// =============================================================================

fn mach_peephole_a64<'a>(ctx: &mut MachContext<'a>, stats: &mut MachStats) {
    // 1a. Self-move elimination (`mov xd, xd`, 64-bit only!).
    if ctx.allow_delete {
        for i in 0..ctx.soa.len() {
            if ctx.soa.is_pinned(i) || ctx.soa.is_opaque_data(i) {
                continue;
            }
            if let DecodedKind::MovReg { rd, rn, wide: true } = ctx.soa.kind_at(i)
                && rd == rn
            {
                ctx.mark_deleted(i);
                stats.movs_eliminated += 1;
            }
        }
        if ctx.dirty_deletes {
            ctx.compact_and_remap();
        }
    }

    // 1b. Compute exact cross-block CFG GPR liveness.
    let mut cfg = compute_live_out_gprs(ctx);
    let mut live_after: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(64, ctx.alloc);

    // 1g-pre. `cset` + `cmp #0` + `csel` flag-forwarding & hazard-checked hoisting.
    // Run before `1d` so eliminating `cset`/`cmp #0` merges NZCV flag epochs and frees `r_flag`!
    if ctx.allow_delete {
        let mut cset_changed = false;
        for m in 0..cfg.num_mbbs() {
            let start = at_usize(&cfg.mbb_start, m);
            let end = at_usize(&cfg.mbb_end, m);
            if start >= end {
                continue;
            }
            let live_out = at_u32(&cfg.live_out, m);
            if try_elim_cset_cmp_csel(ctx, &cfg, m, start, end, live_out, &mut live_after, stats) {
                cset_changed = true;
            }
        }
        if cset_changed {
            ctx.compact_and_remap();
            cfg = compute_live_out_gprs(ctx);
        }
    }

    // 1d. Live-range anti-dependency `csel` rename & latch `mov x_dst, x_tmp` elimination.
    // Run before general copy-prop so latch `mov x_dst, x_tmp` is eliminated directly!
    // Bounded to 4 rounds so each rewrite compacts & refreshes exact CFG liveness.
    if ctx.allow_delete {
        for _ in 0..4 {
            if !try_elim_csel_latch_movs(ctx, &cfg, &mut live_after, stats) {
                break;
            }
            ctx.compact_and_remap();
            cfg = compute_live_out_gprs(ctx);
        }
    }

    // 1c + 1f(store-load reload) + 1a(dead-mov) + 1e(forward & backward copy-prop) + 1g per MBB.
    for m in 0..cfg.num_mbbs() {
        let start = at_usize(&cfg.mbb_start, m);
        let end = at_usize(&cfg.mbb_end, m);
        if start >= end {
            continue;
        }
        let live_out = at_u32(&cfg.live_out, m);

        // 1c. `sub rd, rn, #1` + `cinc rd, rd, cc` (+ `cmp rd, #0`) commutation & Z-flag fusion,
        // flag-setting fusion (`add/sub rd, ...` + `cmp rd, #0` -> `adds/subs rd, ...`), and
        // redundant `cmp rd, #0` elimination after `adds`/`subs`/`ands`/`bics`,
        // ONLY when downstream flag readers exclusively test `N`/`Z` (`Eq`/`Ne`/`Mi`/`Pl`),
        // since `adds`/`subs`/`ands` differ from `cmp rd, #0` on `C` and `V`!
        peephole_commute_sub1_cinc(ctx, start, end, stats);
        if ctx.allow_delete {
            peephole_sink_sub_into_cmp(ctx, start, end, stats);
            peephole_fuse_addsub_cmp_zero(ctx, start, end);
            try_elim_redundant_cmp_zero(ctx, start, end, stats);
        }

        // 1f. Redundant straight-line store-to-load reload elimination (`str` + `ldr` same slot).
        // Runs before copy-prop so any reload rewritten to `mov x_rt2, x_rt` can also propagate.
        try_elim_store_load_reload(ctx, start, end, stats);

        // 1a (dead pure mov) + 1e (backward producer retarget & forward copy propagation).
        if ctx.allow_delete {
            peephole_copy_prop_mbb(ctx, start, end, live_out, &mut live_after, stats);

            // 1h. Bitfield shift+extract fusion (`lsr + ubfx -> ubfx`) and dead narrow `ubfx #0, #{8,16}` elision.
            peephole_fuse_lsr_ubfx(ctx, start, end, live_out, &mut live_after, stats);
            peephole_elide_dead_narrow_ubfx(ctx, start, end, live_out, stats);

            // Second quick sweep in case copy-prop exposed a `sub #1`+`cinc`, `sub+cmp`, redundant `cmp #0`, or `cset+cmp+csel`.
            peephole_commute_sub1_cinc(ctx, start, end, stats);
            peephole_sink_sub_into_cmp(ctx, start, end, stats);
            try_elim_redundant_cmp_zero(ctx, start, end, stats);
            try_elim_cset_cmp_csel(ctx, &cfg, m, start, end, live_out, &mut live_after, stats);
        }
    }
}

/// Decodes an AArch64 `cinc rd, rn, cc` instruction (`csinc rd, rn, rn, invert(cc)` with
/// `rd < 31`, `rn < 31`, and `invert(cc) < 14`), returning `Some((rd, rn, wide))`.
#[inline]
const fn decode_cinc(w: u32) -> Option<(u8, u8, bool)> {
    // `csinc`: `sf 0 0 11010100 Rm cond 01 Rn Rd`
    if (w & 0x7fe0_0c00) != 0x1a80_0400 {
        return None;
    }
    let inv_cond = u4_low(w >> 12);
    if inv_cond >= 14 {
        return None;
    }
    let rm = reg5(w >> 16);
    let rn = reg5(w >> 5);
    let rd = reg5(w);
    if rd >= 31 || rn >= 31 || rm != rn {
        return None;
    }
    let wide = (w >> 31) != 0;
    Some((rd, rn, wide))
}

/// True iff `w` is an AArch64 flag-reading instruction whose condition inspects ONLY the `Z`
/// flag (`Eq = 0` or `Ne = 1`): `csel`/`csinc`/`csinv`/`csneg` (including `cset`/`cinc`/`cinv`/
/// `cneg`) or `b.eq`/`b.ne`.
#[inline]
const fn reads_only_z_flag(w: u32) -> bool {
    // Conditional select family (`csel`/`csinc`/`csinv`/`csneg`):
    // `sf op S=0 11010100 Rm cond op2(0x) Rn Rd`
    if (w & 0x3fe0_0800) == 0x1a80_0000 {
        return ((w >> 12) & 0xf) <= 1;
    }
    // Conditional branch `b.cond`: `01010100 imm19 0 cond`
    if (w & 0xff00_0010) == 0x5400_0000 {
        return (w & 0xf) <= 1;
    }
    false
}

/// Checks whether every consumer of the NZCV flags set at `cmp_idx` in `[cmp_idx + 1 .. end)`
/// inspects ONLY the `Z` flag (`Eq`/`Ne`), and flags do NOT escape the MBB live.
fn flags_consumers_only_z(soa: &MachFuncSoa<'_>, cmp_idx: usize, end: usize) -> bool {
    let mut saw_consumer = false;
    for k in (cmp_idx + 1)..end {
        if soa.is_deleted(k) {
            continue;
        }
        if soa.reads_flags(k) {
            if !reads_only_z_flag(at_u32(&soa.words, k)) {
                return false;
            }
            saw_consumer = true;
        }
        if soa.writes_flags(k) {
            // Flags overwritten before exiting MBB: all consumers verified above.
            return saw_consumer;
        }
        if soa.is_branch_or_barrier(k) {
            // Flag-reading `b.eq`/`b.ne` already verified above; any other branch/barrier
            // (`b`, `cbz`, `ret`, `call`, unknown) may escape with flags live.
            return saw_consumer && matches!(soa.kind_at(k), DecodedKind::BranchCond { is_flag_cond: true, .. });
        }
    }
    false
}

/// Commutes `sub rd, rn, #1; ...; cinc rd, rd, cc` into `cinc rd, rn, cc; ...; sub[s] rd, rd, #1`
/// (exact modular arithmetic `(rn - 1) + (cc ? 1 : 0) == (rn + (cc ? 1 : 0)) - 1 (mod 2^W)`),
/// hoisting `cinc` immediately after the flag producer and — when a matching `cmp rd, #0` follows
/// whose flag consumers inspect ONLY `Z` (`eq`/`ne`) — upgrading `sub` to `subs` and deleting
/// `cmp rd, #0`.
fn peephole_commute_sub1_cinc(
    ctx: &mut MachContext<'_>,
    start: usize,
    end: usize,
    stats: &mut MachStats,
) {
    for i in start..end {
        if ctx.soa.is_deleted(i) || ctx.soa.is_pinned(i) || ctx.soa.is_opaque_data(i) {
            continue;
        }
        let DecodedKind::AddSubImm {
            rd,
            rn,
            imm12: 1,
            shift12: false,
            is_sub: true,
            wide,
            sets_flags: false,
        } = ctx.soa.kind_at(i)
        else {
            continue;
        };
        if rd >= 31 || rn >= 31 {
            continue;
        }
        let rd_bit = 1u32 << rd;
        let rn_bit = 1u32 << rn;

        // 1. Find matching `cinc rd, rd, cc` (same width) in `(i + 1)..end` with no intervening
        //    def of `rd`/`rn`, use of `rd`, flag write, branch/barrier, or trap/reloc.
        let mut j_opt: Option<usize> = None;
        let scan_end = end.min(i + 16);
        for k in (i + 1)..scan_end {
            if ctx.soa.is_deleted(k) {
                continue;
            }
            if ctx.soa.is_target_boundary(k) || ctx.soa.is_opaque_data(k) || ctx.soa.is_reloc_or_trap(k) {
                break;
            }
            if !ctx.soa.is_pinned(k)
                && let Some((cinc_rd, cinc_rn, cinc_wide)) = decode_cinc(at_u32(&ctx.soa.words, k))
                && cinc_rd == rd
                && cinc_rn == rd
                && cinc_wide == wide
            {
                j_opt = Some(k);
                break;
            }
            if ctx.soa.writes_flags(k)
                || ctx.soa.is_branch_or_barrier(k)
                || (at_u32(&ctx.soa.gpr_defs, k) & (rd_bit | rn_bit)) != 0
                || (at_u32(&ctx.soa.gpr_uses, k) & rd_bit) != 0
            {
                break;
            }
        }
        let Some(j) = j_opt else {
            continue;
        };

        // 2. Check if a matching `cmp rd, #0` (same width) follows `j` with no intervening
        //    flag read/write, `rd` write, or barrier, AND all its flag consumers read ONLY `Z`.
        let mut cmp_to_delete: Option<usize> = None;
        if ctx.allow_delete {
            let cmp_scan_end = end.min(j + 16);
            for c in (j + 1)..cmp_scan_end {
                if ctx.soa.is_deleted(c) {
                    continue;
                }
                if ctx.soa.is_target_boundary(c) || ctx.soa.is_opaque_data(c) {
                    break;
                }
                if !ctx.soa.is_pinned(c)
                    && let DecodedKind::CmpZero { rn: cmp_rn, wide: cmp_wide } = ctx.soa.kind_at(c)
                    && cmp_rn == rd
                    && cmp_wide == wide
                {
                    if flags_consumers_only_z(&ctx.soa, c, end) {
                        cmp_to_delete = Some(c);
                    }
                    break;
                }
                if ctx.soa.reads_flags(c)
                    || ctx.soa.writes_flags(c)
                    || ctx.soa.is_branch_or_barrier(c)
                    || (at_u32(&ctx.soa.gpr_defs, c) & rd_bit) != 0
                {
                    break;
                }
            }
        }

        // 3. Apply the sound commute:
        //    - `[i]` <- `cinc rd, rn, cc` (rewrite both `Rm` and `Rn` fields of `csinc` from `rd` to `rn`)
        //    - `[j]` <- `subs rd, rd, #1` if deleting `cmp rd, #0`, else non-flag-setting `sub rd, rd, #1`
        let cinc_word = at_u32(&ctx.soa.words, j);
        let hoisted_cinc = set_field_5(set_field_5(cinc_word, 16, rn), 5, rn);
        let rd_u32 = u32::from(rd);
        let sunk_sub = if let Some(c) = cmp_to_delete {
            ctx.mark_deleted(c);
            stats.movs_eliminated += 1;
            enc::sf(wide, enc::subs_imm(rd_u32, rd_u32, 1, false))
        } else {
            enc::sf(wide, enc::sub_imm(rd_u32, rd_u32, 1, false))
        };
        ctx.soa.replace_word(i, hoisted_cinc);
        ctx.soa.replace_word(j, sunk_sub);
    }
}

/// Fuses a non-flag-setting `sub rd, rn, #imm` (`rd < 31, rn < 31, rd != rn`) with a subsequent
/// matching `cmp rn, #imm` (`subs wzr/xzr, rn, #imm`, same width/shift/imm) inside the same MBB
/// when no intervening instruction reads/writes `rd` or writes `rn`: deletes `sub` and rewrites
/// `cmp` to `subs rd, rn, #imm` (exact same `rd` value and identical NZCV flags, -1 instruction and
/// shorter `rd` live range even across intervening flag writes).
///
/// `rd != rn` is mandatory: when `rd == rn`, `sub rn, rn, #imm` mutates `rn` BEFORE `cmp rn, #imm`,
/// so `cmp` compares `old_rn - imm` against `imm` (not `old_rn` against `imm`), and fusing into
/// `subs rn, rn, #imm` would be off by `imm` (causing unsigned underflow in `while rem >= k`).
fn peephole_sink_sub_into_cmp(
    ctx: &mut MachContext<'_>,
    start: usize,
    end: usize,
    stats: &mut MachStats,
) {
    if !ctx.allow_delete {
        return;
    }
    for i in start..end {
        if ctx.soa.is_deleted(i) || ctx.soa.is_pinned(i) {
            continue;
        }
        let DecodedKind::AddSubImm { rd, rn, imm12, shift12, is_sub: true, wide, sets_flags: false } =
            ctx.soa.kind_at(i)
        else {
            continue;
        };
        if rd >= 31 || rn >= 31 || rd == rn {
            continue;
        }
        let rd_bit = gpr_bit_zr(rd);
        let rn_bit = gpr_bit_zr(rn);
        let scan_end = end.min(i + 24);
        for j in (i + 1)..scan_end {
            if ctx.soa.is_deleted(j) {
                continue;
            }
            if ctx.soa.is_target_boundary(j)
                || ctx.soa.is_opaque_data(j)
                || ctx.soa.is_branch_or_barrier(j)
            {
                break;
            }
            if !ctx.soa.is_pinned(j)
                && let DecodedKind::AddSubImm {
                    rd: 31,
                    rn: jrn,
                    imm12: jimm,
                    shift12: jsh,
                    is_sub: true,
                    wide: jwide,
                    sets_flags: true,
                } = ctx.soa.kind_at(j)
                && jrn == rn
                && jimm == imm12
                && jsh == shift12
                && jwide == wide
            {
                let fused = enc::sf(wide, enc::subs_imm(u32::from(rd), u32::from(rn), u32::from(imm12), shift12));
                ctx.mark_deleted(i);
                ctx.soa.replace_word(j, fused);
                stats.movs_eliminated += 1;
                break;
            }
            let defs = at_u32(&ctx.soa.gpr_defs, j);
            let uses = at_u32(&ctx.soa.gpr_uses, j);
            if ((defs | uses) & rd_bit) != 0 || (defs & rn_bit) != 0 {
                break;
            }
        }
    }
}

/// True iff condition code `cond` depends ONLY on the `N` and/or `Z` flags (never `C` or `V`).
#[inline]
const fn cond_only_nz(cond: u8) -> bool {
    matches!(cond, 0 | 1 | 4 | 5) // Eq(0), Ne(1), Mi(4), Pl(5)
}

/// Checks whether every consumer of the NZCV flags set at `cmp_idx` in `[cmp_idx + 1 .. end)`
/// only inspects `N`/`Z` (`Eq`/`Ne`/`Mi`/`Pl`), and flags are NOT live out past the MBB.
fn flags_consumers_only_nz(soa: &MachFuncSoa<'_>, cmp_idx: usize, end: usize) -> bool {
    let mut saw_consumer = false;
    for k in (cmp_idx + 1)..end {
        if soa.is_deleted(k) {
            continue;
        }
        if soa.reads_flags(k) {
            match soa.kind_at(k) {
                DecodedKind::BranchCond { is_flag_cond: true, cond, .. } => {
                    if !cond_only_nz(cond) {
                        return false;
                    }
                    saw_consumer = true;
                }
                DecodedKind::Csel { cond, .. } | DecodedKind::Cset { cond, .. } => {
                    if !cond_only_nz(cond) {
                        return false;
                    }
                    saw_consumer = true;
                }
                DecodedKind::Ret => {
                    // `Conv::BlazeResult` returns error flag in `C`; if a `ret` follows without
                    // a closer flag write, refuse to fuse unless a consumer already used it and
                    // we know `C` isn't observed. Conservatively refuse if `ret` is reached with
                    // these flags still live!
                    return false;
                }
                _ => return false,
            }
        }
        if soa.writes_flags(k) {
            // Flags overwritten before exiting MBB: all consumers observed above.
            return saw_consumer;
        }
        if soa.is_branch_or_barrier(k) {
            // Branch ended the MBB (e.g. `b.ne` already checked above) or `b` / `cbz` / `call`.
            return saw_consumer && matches!(soa.kind_at(k), DecodedKind::BranchCond { is_flag_cond: true, .. });
        }
    }
    false
}

fn peephole_fuse_addsub_cmp_zero(ctx: &mut MachContext<'_>, start: usize, end: usize) {
    for i in start..end {
        if ctx.soa.is_deleted(i) || ctx.soa.is_pinned(i) || ctx.soa.is_opaque_data(i) {
            continue;
        }
        let (rd, wide) = match ctx.soa.kind_at(i) {
            DecodedKind::AddSubImm { rd, wide, sets_flags: false, .. } if rd < 31 => (rd, wide),
            DecodedKind::AddSubReg { rd, wide, sets_flags: false, .. } if rd < 31 => (rd, wide),
            _ => continue,
        };
        let rd_bit = 1u32 << rd;
        let scan_end = end.min(i + 6);
        for j in (i + 1)..scan_end {
            if ctx.soa.is_deleted(j) {
                continue;
            }
            if ctx.soa.is_pinned(j) || ctx.soa.is_opaque_data(j) {
                break;
            }
            if let DecodedKind::CmpZero { rn, wide: cmp_wide } = ctx.soa.kind_at(j)
                && rn == rd
                && cmp_wide == wide
            {
                if flags_consumers_only_nz(&ctx.soa, j, end) {
                    // Set `S` bit (bit 29) on `add`/`sub` -> `adds`/`subs`, delete `cmp rd, #0`.
                    let fused_word = at_u32(&ctx.soa.words, i) | (1u32 << 29);
                    ctx.soa.replace_word(i, fused_word);
                    ctx.mark_deleted(j);
                }
                break;
            }
            // Any intervening flag reader/writer, `rd` writer, or barrier blocks fusion.
            if ctx.soa.reads_flags(j)
                || ctx.soa.writes_flags(j)
                || ctx.soa.is_branch_or_barrier(j)
                || (at_u32(&ctx.soa.gpr_defs, j) & rd_bit) != 0
            {
                break;
            }
        }
    }
}

/// If instruction `idx` writes `rd < 31` AND sets `N` and `Z` flags identically to
/// `cmp rd, #0` at width `wide` (`adds`/`subs` imm/reg/ext, `ands` imm/reg, `bics` reg),
/// returns `Some((rd, wide))`.
fn nz_flag_producer_rd_wide(soa: &MachFuncSoa<'_>, idx: usize) -> Option<(u8, bool)> {
    if !soa.writes_flags(idx) || soa.is_deleted(idx) || soa.is_opaque_data(idx) {
        return None;
    }
    match soa.kind_at(idx) {
        DecodedKind::AddSubImm { rd, wide, sets_flags: true, .. }
        | DecodedKind::AddSubReg { rd, wide, sets_flags: true, .. }
            if rd < 31 =>
        {
            Some((rd, wide))
        }
        DecodedKind::Other => {
            let w = at_u32(&soa.words, idx);
            let wide = (w >> 31) != 0;
            let rd = reg5(w);
            if rd >= 31 {
                return None;
            }
            // `ands` (immediate): sf 11 100100 N immr imms Rn Rd
            let is_ands_imm = (w & 0x7f80_0000) == 0x7200_0000 && (wide || ((w >> 22) & 1) == 0);
            // `ands` / `bics` (shifted register): sf 11 01010 shift N Rm imm6 Rn Rd
            let is_ands_bics_reg = (w & 0x7f00_0000) == 0x6a00_0000 && (wide || ((w >> 10) & 0x3f) < 32);
            // `adds` / `subs` (shifted register): sf op 1 01011 shift 0 Rm imm6 Rn Rd
            let is_addsub_shifted = (w & 0x3f20_0000) == 0x2b00_0000
                && ((w >> 22) & 3) != 3
                && (wide || ((w >> 10) & 0x3f) < 32);
            // `adds` / `subs` (extended register): sf op 1 01011 00 1 Rm option imm3 Rn Rd
            let is_addsub_ext = (w & 0x3fe0_0000) == 0x2b20_0000 && ((w >> 10) & 7) <= 4;
            if is_ands_imm || is_ands_bics_reg || is_addsub_shifted || is_addsub_ext {
                Some((rd, wide))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Pass 1c-2: Eliminates redundant `cmp rd, #0` (`subs xzr/wzr, rd, #0`) within an 8-instruction
/// straight-line window after a flag-setting `adds`/`subs`/`ands`/`bics` writing `rd` at the same
/// width, when no intervening instruction writes `rd` or NZCV and every downstream flag consumer
/// observes only `N`/`Z` (`Eq`/`Ne`/`Mi`/`Pl`).
fn try_elim_redundant_cmp_zero(
    ctx: &mut MachContext<'_>,
    start: usize,
    end: usize,
    stats: &mut MachStats,
) {
    for i in start..end {
        let Some((rd, wide)) = nz_flag_producer_rd_wide(&ctx.soa, i) else {
            continue;
        };
        let rd_bit = 1u32 << rd;
        let scan_end = end.min(i + 9);
        for k in (i + 1)..scan_end {
            if ctx.soa.is_deleted(k) {
                continue;
            }
            if ctx.soa.is_target_boundary(k) || ctx.soa.is_opaque_data(k) {
                break;
            }
            if !ctx.soa.is_pinned(k)
                && let DecodedKind::CmpZero { rn, wide: cmp_wide } = ctx.soa.kind_at(k)
                && rn == rd
                && cmp_wide == wide
            {
                if flags_consumers_only_nz(&ctx.soa, k, end) {
                    ctx.mark_deleted(k);
                    stats.movs_eliminated += 1;
                    continue;
                }
                break;
            }
            if ctx.soa.writes_flags(k)
                || ctx.soa.is_branch_or_barrier(k)
                || (at_u32(&ctx.soa.gpr_defs, k) & rd_bit) != 0
            {
                break;
            }
        }
    }
}

/// Decodes an AArch64 `ubfm` (`sf 10 100110 N immr imms Rn Rd` with `N == sf`, and for 32-bit
/// `immr < 32 && imms < 32`), returning `Some((wide, rd, rn, immr, imms))`.
#[inline]
const fn decode_ubfm(w: u32) -> Option<(bool, u8, u8, u32, u32)> {
    let top = w & 0xffc0_0000;
    let wide = if top == 0xd340_0000 {
        true
    } else if top == 0x5300_0000 {
        false
    } else {
        return None;
    };
    let immr = (w >> 16) & 0x3f;
    let imms = (w >> 10) & 0x3f;
    if !wide && (immr >= 32 || imms >= 32) {
        return None;
    }
    let rn = reg5(w >> 5);
    let rd = reg5(w);
    Some((wide, rd, rn, immr, imms))
}

/// Fuses `lsr rd, rn, #sh` (32-bit `ubfm wd, wn, #sh, #31` or 64-bit `ubfm xd, xn, #sh, #63`)
/// followed in the same MBB (with no intervening read/write of `rd` or write of `rn`) by
/// `ubfx rd2, rd, #lsb2, #w` (`ubfm` with `imms >= immr`, `rd2 == rd` or `rd` dead after `ubfx`,
/// and `sh + lsb2 + w <= lsr_width`) into a single `ubfx rd2, rn, #(sh + lsb2), #w`.
///
/// Uses the 32-bit `ubfm_w` encoding whenever `sh + lsb2 + w <= 32` since AArch64 32-bit `ubfm`
/// zero-extends its result to 64 bits.
fn peephole_fuse_lsr_ubfx<'a>(
    ctx: &mut MachContext<'a>,
    start: usize,
    end: usize,
    live_out: u32,
    live_after: &mut Vec<u32, Alloc<'a>>,
    stats: &mut MachStats,
) {
    let mut need_live = true;
    for i in start..end {
        if ctx.soa.is_deleted(i) || ctx.soa.is_pinned(i) || ctx.soa.is_opaque_data(i) {
            continue;
        }
        let Some((lsr_wide, rd, rn, sh, lsr_imms)) = decode_ubfm(at_u32(&ctx.soa.words, i)) else {
            continue;
        };
        let lsr_width = if lsr_wide { 64u32 } else { 32u32 };
        if lsr_imms != lsr_width - 1 || rd >= 31 || rn >= 31 {
            continue;
        }
        let rd_bit = 1u32 << rd;
        let rn_bit = 1u32 << rn;

        for k in (i + 1)..end {
            if ctx.soa.is_deleted(k) {
                continue;
            }
            if ctx.soa.is_target_boundary(k) || ctx.soa.is_opaque_data(k) || ctx.soa.is_reloc_or_trap(k) {
                break;
            }
            if !ctx.soa.is_pinned(k)
                && let Some((_ubfx_wide, ubfx_rd, ubfx_rn, ubfx_immr, ubfx_imms)) =
                    decode_ubfm(at_u32(&ctx.soa.words, k))
                && ubfx_rn == rd
                && ubfx_rd < 31
                && ubfx_imms >= ubfx_immr
            {
                let w = ubfx_imms - ubfx_immr + 1;
                let new_immr = sh + ubfx_immr;
                if new_immr + w <= lsr_width {
                    let rd_dead = if ubfx_rd == rd {
                        true
                    } else {
                        if need_live {
                            compute_mbb_live_after(&ctx.soa, start, end, live_out, live_after);
                            need_live = false;
                        }
                        (at_u32(live_after, k - start) & rd_bit) == 0
                    };
                    if rd_dead {
                        let new_imms = new_immr + w - 1;
                        let fused = if new_imms < 32 {
                            enc::ubfm_w(u32::from(ubfx_rd), u32::from(rn), new_immr, new_imms)
                        } else {
                            enc::ubfm(u32::from(ubfx_rd), u32::from(rn), new_immr, new_imms)
                        };
                        ctx.mark_deleted(i);
                        ctx.soa.replace_word(k, fused);
                        stats.movs_eliminated += 1;
                        need_live = true;
                        break;
                    }
                }
            }
            if ctx.soa.is_branch_or_barrier(k)
                || (at_u32(&ctx.soa.gpr_uses, k) & rd_bit) != 0
                || (at_u32(&ctx.soa.gpr_defs, k) & (rd_bit | rn_bit)) != 0
            {
                break;
            }
        }
    }
}

/// If instruction word `w` reads `rd` (`< 31`) strictly in a slot that observes ONLY the low
/// `width` bits (`width` in `{8, 16}`), does NOT read `rd` in any other operand, and rewriting
/// that operand from `rd` to `rn_new` (`< 31`) is 100% ARMv8-legal, returns `Some(rewritten_word)`.
#[inline]
fn rewrite_narrow_ubfx_use(w: u32, rd: u8, rn_new: u8, width: u32) -> Option<u32> {
    if rd >= 31 || rn_new >= 31 || (width != 8 && width != 16) {
        return None;
    }
    let expected_size = if width == 8 { 0u32 } else { 1u32 };
    let size_prefix = expected_size << 30;

    // 1a. Unsigned-offset `strb` (`size=0`) / `strh` (`size=1`):
    //     `size 111 V=0 01 opc=00 imm12 Rn Rt`
    if (w & 0xffc0_0000) == (size_prefix | 0x3900_0000) {
        let rn = reg5(w >> 5);
        let rt = reg5(w);
        if rt == rd && rn != rd {
            return Some(set_field_5(w, 0, rn_new));
        }
        return None;
    }
    // 1b. Unscaled-offset `sturb` (`size=0`) / `sturh` (`size=1`):
    //     `size 111 V=0 00 opc=00 0 imm9 00 Rn Rt`
    if (w & 0xffe0_0c00) == (size_prefix | 0x3800_0000) {
        let rn = reg5(w >> 5);
        let rt = reg5(w);
        if rt == rd && rn != rd {
            return Some(set_field_5(w, 0, rn_new));
        }
        return None;
    }
    // 1c. Pre-/post-index writeback `strb` / `strh`:
    //     `size 111 V=0 00 opc=00 0 imm9 (01|11) Rn Rt`
    //     ARMv8 CONSTRAINED UNPREDICTABLE if `Rt == Rn` with writeback, so require `rn != rn_new`.
    if (w & 0xffe0_0c00) == (size_prefix | 0x3800_0400)
        || (w & 0xffe0_0c00) == (size_prefix | 0x3800_0c00)
    {
        let rn = reg5(w >> 5);
        let rt = reg5(w);
        if rt == rd && rn != rd && rn != rn_new {
            return Some(set_field_5(w, 0, rn_new));
        }
        return None;
    }
    // 1d. Register-offset `strb` / `strh`:
    //     `size 111 V=0 00 opc=00 1 Rm option S 10 Rn Rt`
    if (w & 0xffe0_0c00) == (size_prefix | 0x3820_0800) {
        let option = (w >> 13) & 7;
        if option == 2 || option == 3 || option == 6 || option == 7 {
            let rm = reg5(w >> 16);
            let rn = reg5(w >> 5);
            let rt = reg5(w);
            if rt == rd && rn != rd && rm != rd {
                return Some(set_field_5(w, 0, rn_new));
            }
        }
        return None;
    }

    // 2. 32-bit `orr wd, wn, w_rd, lsl #imm6` (`sf=0, opc=01, shift=LSL(00), N=0`):
    //    `0 01 01010 00 0 Rm imm6 Rn Rd`
    //    Observes only bits `[31 - imm6 : 0]` of `Rm`, so `imm6` in `(32 - width)..32`
    //    (`24..32` for `w == 8`, `16..32` for `w == 16`) strictly reads within the low `width` bits.
    if (w & 0xffe0_0000) == 0x2a00_0000 {
        let imm6 = (w >> 10) & 0x3f;
        let rm = reg5(w >> 16);
        let rn = reg5(w >> 5);
        if ((32 - width)..32).contains(&imm6) && rm == rd && rn != rd {
            return Some(set_field_5(w, 16, rn_new));
        }
        return None;
    }

    // 3. `ubfm` reading strictly within bits `[width - 1 : 0]` of `Rn` (including 32-bit
    //    `lsl wd, w_rd, #24` = `ubfm_w(wd, w_rd, 8, 7)` and `lsl wd, w_rd, #16` = `ubfm_w(wd, w_rd, 16, 15)`):
    //    In ARMv8 `UBFM`, both `UBFX` (`imms >= immr`) and `UBFIZ`/`LSL` (`imms < immr`) never
    //    observe any bit of `Rn` above bit `imms`.
    if let Some((_wide, _ubfm_rd, ubfm_rn, _immr, imms)) = decode_ubfm(w)
        && ubfm_rn == rd
        && imms < width
    {
        return Some(set_field_5(w, 5, rn_new));
    }

    None
}

/// Eliminates `ubfx rd, rn, #0, #w` (`w` in `{8, 16}`, 32-bit or 64-bit `ubfm`) when `rd` is
/// dead after its uses in the MBB (either completely overwritten in the MBB or not in `live_out`)
/// and EVERY read of `rd` strictly observes only its low `w` bits (`strb`/`sturb`/`strh`/`sturh`
/// in `Rt`, 32-bit `orr` with `lsl #(>= 32 - w)` in `Rm`, or `ubfm` with `imms < w` in `Rn`).
fn peephole_elide_dead_narrow_ubfx(
    ctx: &mut MachContext<'_>,
    start: usize,
    end: usize,
    live_out: u32,
    stats: &mut MachStats,
) {
    for i in start..end {
        if ctx.soa.is_deleted(i) || ctx.soa.is_pinned(i) || ctx.soa.is_opaque_data(i) {
            continue;
        }
        let Some((_wide, rd, rn, immr, imms)) = decode_ubfm(at_u32(&ctx.soa.words, i)) else {
            continue;
        };
        if immr != 0 || rd >= 31 || rn >= 31 {
            continue;
        }
        let width = match imms {
            7 => 8u32,
            15 => 16u32,
            _ => continue,
        };
        let rd_bit = 1u32 << rd;
        let rn_bit = 1u32 << rn;

        // Pass 1 (read-only verification): verify every use of `rd` up to its complete redef
        // (or MBB end if dead in `live_out`) strictly observes only the low `width` bits.
        let mut ok = true;
        let mut rd_killed = false;
        let mut rn_clobbered = false;
        let mut last_use = i;
        for k in (i + 1)..end {
            if ctx.soa.is_deleted(k) {
                continue;
            }
            if ctx.soa.is_target_boundary(k) || ctx.soa.is_opaque_data(k) {
                ok = false;
                break;
            }
            let k_uses = at_u32(&ctx.soa.gpr_uses, k);
            let k_defs = at_u32(&ctx.soa.gpr_defs, k);
            if (k_uses & rd_bit) != 0 {
                if (rd != rn && (rn_clobbered || ctx.soa.is_pinned(k)))
                    || rewrite_narrow_ubfx_use(at_u32(&ctx.soa.words, k), rd, rn, width).is_none()
                {
                    ok = false;
                    break;
                }
                last_use = k;
            }
            if (k_defs & rd_bit) != 0 {
                // Any instruction that writes `rd` without failing the use check above completely
                // overwrites `rd` (partial defs like `movk`/`bfm` set `gpr_uses |= rd_bit` and
                // are rejected by `rewrite_narrow_ubfx_use`).
                rd_killed = true;
                break;
            }
            if rd != rn && (k_defs & rn_bit) != 0 {
                rn_clobbered = true;
            }
        }
        if !ok || (!rd_killed && (live_out & rd_bit) != 0) {
            continue;
        }

        // Pass 2 (commit): rewrite verified narrow uses from `rd` -> `rn` (no-op when `rd == rn`)
        // and delete the dead `ubfx`.
        if rd != rn {
            for k in (i + 1)..=last_use {
                if ctx.soa.is_deleted(k) {
                    continue;
                }
                if (at_u32(&ctx.soa.gpr_uses, k) & rd_bit) != 0
                    && let Some(nw) = rewrite_narrow_ubfx_use(at_u32(&ctx.soa.words, k), rd, rn, width)
                {
                    ctx.soa.replace_word(k, nw);
                }
            }
        }
        ctx.mark_deleted(i);
        stats.movs_eliminated += 1;
    }
}

fn peephole_copy_prop_mbb<'a>(
    ctx: &mut MachContext<'a>,
    start: usize,
    end: usize,
    live_out: u32,
    live_after: &mut Vec<u32, Alloc<'a>>,
    stats: &mut MachStats,
) {
    let mut changed = true;
    let mut rounds = 0usize;
    while changed && rounds < 4 {
        changed = false;
        rounds += 1;
        compute_mbb_live_after(&ctx.soa, start, end, live_out, live_after);

        for i in start..end {
            if ctx.soa.is_deleted(i) || ctx.soa.is_pinned(i) || ctx.soa.is_opaque_data(i) {
                continue;
            }

            // (1) Dead pure `MovReg` elimination (`rd` dead immediately after `i`).
            if let DecodedKind::MovReg { rd, rn, wide } = ctx.soa.kind_at(i) {
                if rd < 31 && (at_u32(live_after, i - start) & (1u32 << rd)) == 0 {
                    ctx.mark_deleted(i);
                    stats.movs_eliminated += 1;
                    changed = true;
                    continue;
                }
                // (2) Forward copy propagation for pure 64-bit `mov rd, xs` (`rd != xs`, both `< 31`).
                if wide && rd < 31 && rn < 31 && rd != rn {
                    let xs = rn;
                    let rd_bit = 1u32 << rd;
                    let xs_bit = 1u32 << xs;
                    let mut can_elim = false;
                    let mut last_rw = i;
                    let mut all_ok = true;
                    for k in (i + 1)..end {
                        if ctx.soa.is_deleted(k) {
                            continue;
                        }
                        let k_uses = at_u32(&ctx.soa.gpr_uses, k);
                        let k_defs = at_u32(&ctx.soa.gpr_defs, k);
                        if (k_uses & rd_bit) != 0 {
                            if rewrite_gpr_in_inst(&ctx.soa, k, rd, xs, true, false).is_none() {
                                all_ok = false;
                                break;
                            }
                            last_rw = k;
                        }
                        if (k_defs & rd_bit) != 0 || (at_u32(live_after, k - start) & rd_bit) == 0 {
                            // `rd` is overwritten or dies at `k` before `xs` is clobbered!
                            can_elim = true;
                            break;
                        }
                        if (k_defs & xs_bit) != 0 || ctx.soa.is_branch_or_barrier(k) {
                            all_ok = false;
                            break;
                        }
                    }
                    if all_ok && can_elim {
                        for k in (i + 1)..=last_rw {
                            if ctx.soa.is_deleted(k) {
                                continue;
                            }
                            if (at_u32(&ctx.soa.gpr_uses, k) & rd_bit) != 0
                                && let Some(nw) = rewrite_gpr_in_inst(&ctx.soa, k, rd, xs, true, false)
                            {
                                ctx.soa.replace_word(k, nw);
                            }
                        }
                        ctx.mark_deleted(i);
                        stats.movs_eliminated += 1;
                        changed = true;
                        continue;
                    }
                }
            }

            // (3) Backward copy propagation: pure producer `op rd, ...` followed by `mov dst, rd`
            // where `rd` is dead after `mov dst, rd`.
            let i_defs = at_u32(&ctx.soa.gpr_defs, i);
            if i_defs.count_ones() == 1 && !ctx.soa.is_branch_or_barrier(i) {
                let rd = reg5(i_defs.trailing_zeros());
                if rd < 31 && can_rewrite_gpr_def(&ctx.soa, i, rd) {
                    let rd_bit = 1u32 << rd;
                    let mut j_found = None;
                    for j in (i + 1)..end {
                        if ctx.soa.is_deleted(j) {
                            continue;
                        }
                        if ctx.soa.is_pinned(j) || ctx.soa.is_opaque_data(j) {
                            break;
                        }
                        if let DecodedKind::MovReg { rd: dst, rn: src, wide: true } = ctx.soa.kind_at(j)
                            && src == rd
                            && dst < 31
                            && dst != rd
                            && (at_u32(live_after, j - start) & rd_bit) == 0
                        {
                            // Verify no intervening instruction in `(i+1)..j` touches `dst` (or `rd`).
                            let dst_bit = 1u32 << dst;
                            let mut dst_clean = true;
                            for mid in (i + 1)..j {
                                if ctx.soa.is_deleted(mid) {
                                    continue;
                                }
                                if (at_u32(&ctx.soa.gpr_uses, mid) & dst_bit) != 0
                                    || (at_u32(&ctx.soa.gpr_defs, mid) & dst_bit) != 0
                                {
                                    dst_clean = false;
                                    break;
                                }
                            }
                            if dst_clean {
                                j_found = Some((j, dst));
                            }
                            break;
                        }
                        if (at_u32(&ctx.soa.gpr_uses, j) & rd_bit) != 0
                            || (at_u32(&ctx.soa.gpr_defs, j) & rd_bit) != 0
                            || ctx.soa.is_branch_or_barrier(j)
                        {
                            break;
                        }
                    }
                    if let Some((j, dst)) = j_found
                        && let Some(nw) = rewrite_gpr_in_inst(&ctx.soa, i, rd, dst, false, true)
                    {
                        ctx.soa.replace_word(i, nw);
                        ctx.mark_deleted(j);
                        stats.movs_eliminated += 1;
                        changed = true;
                    }
                }
            }
        }
    }
}

/// Backward-BFS proof for cross-MBB latch `mov x_dst, x_tmp` elimination.
///
/// Returns `Some(def_mbb)` iff EVERY CFG path reaching `l_mbb` originates at a single
/// `def_mbb != l_mbb` (that defines `x_tmp`), traversing only clean pass-through MBBs
/// (never reaching function entry `0` without passing through `def_mbb`, never touching
/// `x_tmp` or `x_dst`, and never having an unknown successor), AND every side-exit leaving
/// `{def_mbb} U pass_through` has neither `x_tmp` nor `x_dst` live-in.
fn find_unique_csel_def_mbb<'a>(
    cfg: &MachCfgLiveness<'a>,
    l_mbb: usize,
    x_dst: u8,
    x_tmp: u8,
    alloc: Alloc<'a>,
) -> Option<usize> {
    if cfg.has_indirect_or_unknown || l_mbb == 0 || at_u32(&cfg.pred_count, l_mbb) == 0 {
        return None;
    }
    let num_mbbs = cfg.num_mbbs();
    let tmp_bit = 1u32 << x_tmp;
    let dst_bit = 1u32 << x_dst;
    let both_bits = tmp_bit | dst_bit;

    // State per MBB: 0 = unvisited, 1 = queued/pass-through, 2 = def_mbb.
    let mut mark = vec_filled_in(num_mbbs, 0u8, alloc);
    let mut worklist: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(16, alloc);
    let mut pass_through: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(16, alloc);
    let mut def_mbb: Option<usize> = None;

    // Seed with all predecessors of `l_mbb`.
    let p_lo = to_idx(at_u32(&cfg.pred_off, l_mbb));
    let p_hi = to_idx(at_u32(&cfg.pred_off, l_mbb + 1));
    for pi in p_lo..p_hi {
        let p = to_idx(at_u32(&cfg.pred_list, pi));
        if p >= num_mbbs || p == l_mbb {
            return None;
        }
        if at_u8(&mark, p) == 0 {
            set_at(&mut mark, p, 1);
            worklist.push(p);
        }
    }

    // Bounded backward BFS (at most `num_mbbs` pops, Rule 18).
    let mut head = 0usize;
    while head < worklist.len() {
        let p = at_usize(&worklist, head);
        head += 1;

        if cfg.unknown_succ.get(p).copied().unwrap_or(true) {
            return None;
        }
        let p_defs = at_u32(&cfg.def_mask, p);
        let p_uses = at_u32(&cfg.use_mask, p);

        if ((p_defs | p_uses) & both_bits) == 0 {
            // Clean pass-through MBB: must NOT be function entry `0` or have 0 predecessors.
            if p == 0 || at_u32(&cfg.pred_count, p) == 0 {
                return None;
            }
            pass_through.push(p);
            let lo = to_idx(at_u32(&cfg.pred_off, p));
            let hi = to_idx(at_u32(&cfg.pred_off, p + 1));
            for pi in lo..hi {
                let pred = to_idx(at_u32(&cfg.pred_list, pi));
                if pred >= num_mbbs || pred == l_mbb {
                    return None;
                }
                if at_u8(&mark, pred) == 0 {
                    set_at(&mut mark, pred, 1);
                    worklist.push(pred);
                }
            }
        } else if (p_defs & tmp_bit) != 0 {
            // Reached a definition of `x_tmp`: must be the UNIQUE `def_mbb`.
            match def_mbb {
                Some(prev_def) if prev_def != p => return None,
                None => def_mbb = Some(p),
                Some(_) => {}
            }
            set_at(&mut mark, p, 2);
        } else {
            // `p` reads `x_tmp` or reads/writes `x_dst` without defining `x_tmp` -> interference!
            return None;
        }
    }

    let d_mbb = def_mbb?;
    if cfg.unknown_succ.get(d_mbb).copied().unwrap_or(true) {
        return None;
    }

    // Forward side-exit check: every successor of `d_mbb` and of every `pass_through` MBB must
    // either stay inside `{l_mbb} U pass_through` (mark == 1) OR not read `x_tmp` or `x_dst`.
    let check_succs = |u: usize| -> bool {
        let s0 = at_u32(&cfg.succ0, u);
        let s1 = at_u32(&cfg.succ1, u);
        for s in [s0, s1] {
            if s == NO_MBB {
                continue;
            }
            let si = to_idx(s);
            if si == l_mbb || at_u8(&mark, si) == 1 {
                continue;
            }
            if (at_u32(&cfg.live_in, si) & both_bits) != 0 {
                return false;
            }
        }
        true
    };

    if !check_succs(d_mbb) {
        return None;
    }
    for &p in &pass_through {
        if !check_succs(p) {
            return None;
        }
    }

    Some(d_mbb)
}

/// Pass 1d: General live-range anti-dependency `csel` rename & reorder.
///
/// Eliminates loop-latch and rotated loop-header-entry copies `mov x_dst, x_tmp` (either later
/// in the same MBB `B`, in a unique-predecessor latch MBB `L`, or across a multi-arm rotated
/// loop latch proved via [`find_unique_csel_def_mbb`]) when `x_tmp` is defined by `csel x_tmp, ...`
/// reading `x_dst`, subsequent instructions in the same NZCV flag epoch also read `x_dst` and
/// overwrite `x_dst` as a dead scratch chain, and a dead register `r_scratch` is available to host
/// that scratch chain.
///
/// Returns `true` immediately after performing one rewrite so the caller compacts and refreshes
/// exact CFG liveness before scanning for additional latch moves.
fn try_elim_csel_latch_movs<'a>(
    ctx: &mut MachContext<'a>,
    cfg: &MachCfgLiveness<'a>,
    live_after_buf: &mut Vec<u32, Alloc<'a>>,
    stats: &mut MachStats,
) -> bool {
    let num_mbbs = cfg.num_mbbs();

    for l_mbb in 0..num_mbbs {
        let l_start = at_usize(&cfg.mbb_start, l_mbb);
        let l_end = at_usize(&cfg.mbb_end, l_mbb);
        if l_start >= l_end {
            continue;
        }
        compute_mbb_live_after(&ctx.soa, l_start, l_end, at_u32(&cfg.live_out, l_mbb), live_after_buf);

        // Find candidate `mov x_dst, x_tmp` in `l_mbb` where `x_tmp` is dead immediately after it.
        for m_idx in l_start..l_end {
            if ctx.soa.is_deleted(m_idx) || ctx.soa.is_pinned(m_idx) || ctx.soa.is_opaque_data(m_idx) {
                continue;
            }
            let DecodedKind::MovReg { rd: x_dst, rn: x_tmp, wide: true } = ctx.soa.kind_at(m_idx) else {
                continue;
            };
            if x_dst >= 31 || x_tmp >= 31 || x_dst == x_tmp {
                continue;
            }
            let tmp_bit = 1u32 << x_tmp;
            let dst_bit = 1u32 << x_dst;
            if (at_u32(live_after_buf, m_idx - l_start) & tmp_bit) != 0 {
                continue;
            }

            // Case 1: Same-MBB latch (`l_mbb` itself contains `csel x_tmp, ...` before `m_idx`).
            if try_csel_rename_in_range(
                ctx,
                cfg,
                l_mbb,
                l_start,
                m_idx,
                m_idx,
                x_dst,
                x_tmp,
                false,
                stats,
            ) {
                return true;
            }

            // Case 2: Cross-MBB latch (direct unique predecessor OR rotated multi-arm loop latch
            // with `mov x_dst, x_tmp` at the head of `l_mbb`).
            // First verify nothing in `l_mbb` before `m_idx` reads/writes `x_tmp` or `x_dst`.
            let mut latch_prefix_clean = true;
            for p in l_start..m_idx {
                if ctx.soa.is_deleted(p) {
                    continue;
                }
                let u = at_u32(&ctx.soa.gpr_uses, p);
                let d = at_u32(&ctx.soa.gpr_defs, p);
                if ((u | d) & (tmp_bit | dst_bit)) != 0 {
                    latch_prefix_clean = false;
                    break;
                }
            }
            if !latch_prefix_clean {
                continue;
            }

            let Some(b_mbb) = find_unique_csel_def_mbb(cfg, l_mbb, x_dst, x_tmp, ctx.alloc) else {
                continue;
            };

            let b_start = at_usize(&cfg.mbb_start, b_mbb);
            let b_end = at_usize(&cfg.mbb_end, b_mbb);
            if try_csel_rename_in_range(
                ctx,
                cfg,
                b_mbb,
                b_start,
                b_end,
                m_idx,
                x_dst,
                x_tmp,
                true,
                stats,
            ) {
                return true;
            }
        }
    }

    false
}

/// Attempts the general anti-dependency `csel` rename inside `[b_start..limit)` of `b_mbb`,
/// deleting the latch move at `mov_idx` upon success.
fn try_csel_rename_in_range<'a>(
    ctx: &mut MachContext<'a>,
    cfg: &MachCfgLiveness<'a>,
    b_mbb: usize,
    b_start: usize,
    limit: usize,
    mov_idx: usize,
    x_dst: u8,
    x_tmp: u8,
    cross_mbb: bool,
    stats: &mut MachStats,
) -> bool {
    if b_start >= limit {
        return false;
    }
    let tmp_bit = 1u32 << x_tmp;
    let dst_bit = 1u32 << x_dst;

    // 1. Find the unique/last definition `k0` of `x_tmp` in `b_start..limit`, which must be a non-pinned `Csel`.
    let mut k0_opt: Option<(usize, u8, u8)> = None;
    for i in (b_start..limit).rev() {
        if ctx.soa.is_deleted(i) {
            continue;
        }
        if (at_u32(&ctx.soa.gpr_uses, i) & tmp_bit) != 0 {
            // `x_tmp` is read inside `b_mbb` after its definition -> cannot eliminate `x_tmp`.
            return false;
        }
        if (at_u32(&ctx.soa.gpr_defs, i) & tmp_bit) != 0 {
            if ctx.soa.is_pinned(i) || ctx.soa.is_opaque_data(i) {
                return false;
            }
            if let DecodedKind::Csel { rd, rn, rm, .. } = ctx.soa.kind_at(i)
                && rd == x_tmp
                && (rn == x_dst || rm == x_dst)
            {
                k0_opt = Some((i, rn, rm));
            }
            break;
        }
    }
    let Some((k0, k0_rn, k0_rm)) = k0_opt else {
        return false;
    };

    // 2. Compute exact `live_after` within `b_mbb` (`b_start..b_end`) using `cfg.live_out[b_mbb]`.
    let b_end = at_usize(&cfg.mbb_end, b_mbb);
    let mut b_live_after: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(b_end.saturating_sub(b_start), ctx.alloc);
    compute_mbb_live_after(&ctx.soa, b_start, b_end, at_u32(&cfg.live_out, b_mbb), &mut b_live_after);

    // 3. Scan `(k0 + 1)..limit` to find:
    //    - `k_ov`: the first instruction that writes `x_dst` (if any)
    //    - `insert_after`: the last instruction reading OLD `x_dst` (must be `<= k_ov` if `k_ov` exists)
    //    - `k_last_use`: the last instruction in `k_ov..limit` reading or writing scratch `x_dst`
    let mut k_ov: Option<usize> = None;
    let mut insert_after = k0;
    let mut k_last_use: Option<usize> = None;

    for i in (k0 + 1)..limit {
        if ctx.soa.is_deleted(i) {
            continue;
        }
        let u = at_u32(&ctx.soa.gpr_uses, i);
        let d = at_u32(&ctx.soa.gpr_defs, i);
        if ((u | d) & tmp_bit) != 0 {
            return false;
        }
        if k_ov.is_none() {
            if (u & dst_bit) != 0 {
                insert_after = i;
            }
            if (d & dst_bit) != 0 {
                k_ov = Some(i);
                k_last_use = Some(i);
            }
        } else if ((u | d) & dst_bit) != 0 {
            k_last_use = Some(i);
        }
    }

    // If `cross_mbb` and `x_dst` was read/written by the trailing terminator of `b_mbb` itself, refuse.
    if cross_mbb && limit > b_start {
        let term = limit - 1;
        if ctx.soa.is_branch_or_barrier(term)
            && ((at_u32(&ctx.soa.gpr_uses, term) | at_u32(&ctx.soa.gpr_defs, term)) & (dst_bit | tmp_bit)) != 0
        {
            return false;
        }
    }

    // 4. Verify `k0` can legally move to `insert_after` within the SAME NZCV flag epoch!
    //    (No instruction in `(k0 + 1)..=insert_after` may write flags, be pinned/barrier,
    //     or write `k0`'s inputs `k0_rn`/`k0_rm` — note that if `i == k_ov`, `k_ov`'s dest
    //     will be renamed from `x_dst` to `r_scratch`, so `k_ov` won't clobber `x_dst`!).
    let k0_in_other = (gpr_bit_zr(k0_rn) | gpr_bit_zr(k0_rm)) & !dst_bit;
    for i in (k0 + 1)..=insert_after {
        if ctx.soa.is_deleted(i)
            || ctx.soa.writes_flags(i)
            || ctx.soa.is_pinned(i)
            || ctx.soa.is_branch_or_barrier(i)
            || ctx.soa.is_opaque_data(i)
        {
            return false;
        }
        let d = at_u32(&ctx.soa.gpr_defs, i);
        if (d & k0_in_other) != 0 {
            return false;
        }
        if (d & dst_bit) != 0 && k_ov != Some(i) {
            return false;
        }
    }

    // 5. If `k_ov` exists, find a provably dead & ABI-legal `r_scratch` and verify every
    //    instruction in `k_ov..=k_last_use` is cleanly rewritable!
    let mut chosen_scratch: Option<u8> = None;
    if let Some(ov) = k_ov {
        let last_u = k_last_use.unwrap_or(ov);
        if !can_rewrite_gpr_def(&ctx.soa, ov, x_dst) {
            return false;
        }
        // If `k_ov` happens BEFORE `insert_after` that shouldn't occur because `insert_after` stops updating at `k_ov`.
        if ov < insert_after {
            return false;
        }
        // Every instruction in `(ov + 1)..=last_u` touching `x_dst` must be cleanly rewritable.
        for p in (ov + 1)..=last_u {
            if ctx.soa.is_deleted(p) {
                continue;
            }
            if ((at_u32(&ctx.soa.gpr_uses, p) | at_u32(&ctx.soa.gpr_defs, p)) & dst_bit) != 0 {
                // Probe with dummy reg 0 (just checking rewritability of the opcode).
                let probe = if x_dst == 0 { 1 } else { 0 };
                if rewrite_gpr_in_inst(&ctx.soa, p, x_dst, probe, true, true).is_none() {
                    return false;
                }
            }
        }

        // Allowed scratch pool: caller-saved `X0..=X15`, plus any callee-saved `X19..=X28` already
        // written in this function (thus already saved/restored by the function's prologue/epilogue).
        let allowed_pool = (CALLER_SCRATCH_GPRS | (CALLEE_SAVED_GPRS & cfg.func_written_gprs))
            & !dst_bit
            & !tmp_bit
            & !k0_in_other;

        // Prefer a register that JUST died at `ov` (e.g. `x19` in `csel x8, x8, x19, eq`), then any dead allowed reg.
        let just_used = at_u32(&ctx.soa.gpr_uses, ov) & allowed_pool;
        let ordered_pools = [just_used, allowed_pool & !just_used];

        'search: for pool in ordered_pools {
            let mut mask = pool;
            while mask != 0 {
                let r = reg5(mask.trailing_zeros());
                mask &= mask - 1;
                let r_bit = 1u32 << r;

                // `r` must be dead immediately after `ov`, and unused/undefined/dead across `(ov + 1)..=last_u`.
                if (at_u32(&b_live_after, ov - b_start) & r_bit) != 0 {
                    continue;
                }
                let mut r_free = true;
                for p in (ov + 1)..=last_u {
                    if ctx.soa.is_deleted(p) {
                        continue;
                    }
                    if ((at_u32(&ctx.soa.gpr_uses, p) | at_u32(&ctx.soa.gpr_defs, p)) & r_bit) != 0
                        || (at_u32(&b_live_after, p - b_start) & r_bit) != 0
                    {
                        r_free = false;
                        break;
                    }
                }
                if r_free {
                    chosen_scratch = Some(r);
                    break 'search;
                }
            }
        }

        if chosen_scratch.is_none() {
            return false;
        }
    }

    // 6. All soundness proofs passed! Apply the 3-step transformation cleanly:
    //    (a) Rename scratch chain `[k_ov ..= k_last_use]` from `x_dst -> r_scratch`.
    if let (Some(ov), Some(r_scratch)) = (k_ov, chosen_scratch) {
        let last_u = k_last_use.unwrap_or(ov);
        let Some(ov_nw) = rewrite_gpr_in_inst(&ctx.soa, ov, x_dst, r_scratch, false, true) else {
            return false;
        };
        ctx.soa.replace_word(ov, ov_nw);
        for p in (ov + 1)..=last_u {
            if ctx.soa.is_deleted(p) {
                continue;
            }
            if ((at_u32(&ctx.soa.gpr_uses, p) | at_u32(&ctx.soa.gpr_defs, p)) & dst_bit) != 0
                && let Some(nw) = rewrite_gpr_in_inst(&ctx.soa, p, x_dst, r_scratch, true, true)
            {
                ctx.soa.replace_word(p, nw);
            }
        }
    }

    //    (b) Retarget `k0`'s `csel` dest `x_tmp -> x_dst` and slide `k0` right after `insert_after`.
    let Some(retargeted_k0) = rewrite_gpr_in_inst(&ctx.soa, k0, x_tmp, x_dst, false, true) else {
        return false;
    };
    for i in k0..insert_after {
        let next_w = at_u32(&ctx.soa.words, i + 1);
        ctx.soa.replace_word(i, next_w);
    }
    ctx.soa.replace_word(insert_after, retargeted_k0);

    //    (c) Delete the latch `mov x_dst, x_tmp`!
    ctx.mark_deleted(mov_idx);
    stats.movs_eliminated += 1;
    true
}

/// Returns `true` iff any surviving instruction in `mbb_idx` could read live-in NZCV flags
/// before overwriting them (ignoring `Ret`, which never relies on a boolean `cmp r_flag, #0`).
fn mbb_reads_live_in_flags(soa: &MachFuncSoa<'_>, cfg: &MachCfgLiveness<'_>, mbb_idx: usize) -> bool {
    let start = at_usize(&cfg.mbb_start, mbb_idx);
    let end = at_usize(&cfg.mbb_end, mbb_idx);
    for idx in start..end {
        if soa.is_deleted(idx) {
            continue;
        }
        if matches!(soa.kind_at(idx), DecodedKind::Ret) {
            return false;
        }
        if soa.reads_flags(idx) {
            return true;
        }
        if soa.writes_flags(idx) {
            return false;
        }
    }
    false
}

/// Pass 1g: `cset` + `cmp #0` + `csel` flag-forwarding & hazard-checked hoisting.
///
/// Eliminates redundant boolean materialization (`cset r_flag, orig_cc` + `cmp r_flag, #0`)
/// when `r_flag` is dead immediately after `cmp #0` and every flag consumer of `cmp #0` is a
/// `csel` testing `Eq`/`Ne` (`cond in {0, 1}`):
/// - **Case 1** (no intervening flag writer in `(i + 1)..k`): rewrites every consuming `csel`
///   to `orig_cc` (for `Ne`) or `orig_cc ^ 1` (for `Eq`), and deletes both `cset` and `cmp #0`.
/// - **Case 2** (intervening flag writer `fw`, single consuming `csel m`): when `[fw..=m]` has
///   no pinned/reloc/trap/boundary/opaque state and no surviving instruction `t` in `fw..m`
///   (excluding `k`) has RAW/WAR/WAW or barrier hazard with `m`, rewrites `m`'s condition and
///   rotates `[fw..=m]` right by 1 so `m` lands right before `fw`, then deletes `cset` and `cmp #0`.
fn try_elim_cset_cmp_csel<'a>(
    ctx: &mut MachContext<'a>,
    cfg: &MachCfgLiveness<'a>,
    mbb_idx: usize,
    mbb_start: usize,
    mbb_end: usize,
    live_out: u32,
    live_after: &mut Vec<u32, Alloc<'a>>,
    stats: &mut MachStats,
) -> bool {
    if !ctx.allow_delete || mbb_start >= mbb_end {
        return false;
    }
    compute_mbb_live_after(&ctx.soa, mbb_start, mbb_end, live_out, live_after);
    let mut changed = false;

    for i in mbb_start..mbb_end {
        if ctx.soa.is_deleted(i) || ctx.soa.is_pinned(i) || ctx.soa.is_opaque_data(i) {
            continue;
        }
        let DecodedKind::Cset { rd: r_flag, cond: orig_cc, .. } = ctx.soa.kind_at(i) else {
            continue;
        };
        if r_flag >= 31 || orig_cc >= 14 {
            continue;
        }
        let r_bit = 1u32 << r_flag;

        // 1. Find the FIRST instruction in `(i + 1)..mbb_end.min(i + 24)` reading or writing `r_flag`.
        let scan_end = mbb_end.min(i + 24);
        let mut k_opt: Option<usize> = None;
        let mut first_fw: Option<usize> = None;
        for j in (i + 1)..scan_end {
            if ctx.soa.is_deleted(j) {
                continue;
            }
            if ctx.soa.is_branch_or_barrier(j) || ctx.soa.is_opaque_data(j) {
                break;
            }
            let j_defs = at_u32(&ctx.soa.gpr_defs, j);
            let j_uses = at_u32(&ctx.soa.gpr_uses, j);
            if (j_defs & r_bit) != 0 {
                break;
            }
            if (j_uses & r_bit) != 0 {
                if !ctx.soa.is_pinned(j)
                    && let DecodedKind::CmpZero { rn, .. } = ctx.soa.kind_at(j)
                    && rn == r_flag
                {
                    k_opt = Some(j);
                }
                break;
            }
            if ctx.soa.writes_flags(j) && first_fw.is_none() {
                first_fw = Some(j);
            }
        }
        let Some(k) = k_opt else {
            continue;
        };

        // 2. Verify `r_flag` is strictly dead immediately after `k`.
        if (at_u32(live_after, k - mbb_start) & r_bit) != 0 {
            continue;
        }

        // 3. Collect ALL flag consumers of `k` up to the next flag writer or non-flag-reading terminator.
        let mut consumers = [0usize; 4];
        let mut consumer_conds = [0u8; 4];
        let mut n_consumers = 0usize;
        let mut flags_closed = false;
        let mut consumers_ok = true;

        for m in (k + 1)..mbb_end {
            if ctx.soa.is_deleted(m) {
                continue;
            }
            if matches!(ctx.soa.kind_at(m), DecodedKind::Ret) {
                flags_closed = true;
                break;
            }
            if ctx.soa.reads_flags(m) {
                if ctx.soa.is_pinned(m) || ctx.soa.is_opaque_data(m) {
                    consumers_ok = false;
                    break;
                }
                if let DecodedKind::Csel { cond, .. } = ctx.soa.kind_at(m)
                    && (cond == 0 || cond == 1)
                    && n_consumers < 4
                {
                    set_at(&mut consumers, n_consumers, m);
                    set_at(&mut consumer_conds, n_consumers, cond);
                    n_consumers += 1;
                } else {
                    consumers_ok = false;
                    break;
                }
            }
            if ctx.soa.writes_flags(m) {
                flags_closed = true;
                break;
            }
            if ctx.soa.is_branch_or_barrier(m) {
                if matches!(
                    ctx.soa.kind_at(m),
                    DecodedKind::BranchUncond { .. } | DecodedKind::BranchCond { is_flag_cond: false, .. }
                ) {
                    let s0 = at_u32(&cfg.succ0, mbb_idx);
                    let s1 = at_u32(&cfg.succ1, mbb_idx);
                    let s0_reads = s0 != NO_MBB && mbb_reads_live_in_flags(&ctx.soa, cfg, to_idx(s0));
                    let s1_reads = s1 != NO_MBB && mbb_reads_live_in_flags(&ctx.soa, cfg, to_idx(s1));
                    if !s0_reads && !s1_reads {
                        flags_closed = true;
                    }
                }
                break;
            }
        }
        if !consumers_ok || !flags_closed || n_consumers == 0 {
            continue;
        }

        match first_fw {
            // Case 1: No intervening flag writer in `(i + 1)..k`.
            None => {
                for c in 0..n_consumers {
                    let m = at_usize(&consumers, c);
                    let csel_cond = at_u8(&consumer_conds, c);
                    let new_cond = if csel_cond == 1 { orig_cc } else { orig_cc ^ 1 };
                    let nw = (at_u32(&ctx.soa.words, m) & !(0xfu32 << 12)) | (u32::from(new_cond & 0xf) << 12);
                    ctx.soa.replace_word(m, nw);
                }
                ctx.mark_deleted(k);
                ctx.mark_deleted(i);
                stats.movs_eliminated += 2;
                changed = true;
                compute_mbb_live_after(&ctx.soa, mbb_start, mbb_end, live_out, live_after);
            }
            // Case 2: Intervening flag writer `fw`, single consuming `csel m` -> hoist `m` before `fw`.
            Some(fw) if n_consumers == 1 => {
                let m = at_usize(&consumers, 0);
                let csel_cond = at_u8(&consumer_conds, 0);
                let m_defs = at_u32(&ctx.soa.gpr_defs, m);
                let m_uses = at_u32(&ctx.soa.gpr_uses, m);

                let forbid_state = STATE_PINNED | STATE_RELOC_OR_TRAP | STATE_TARGET_BOUNDARY | STATE_OPAQUE_DATA;
                let mut hoist_ok = true;
                for idx in fw..=m {
                    if (at_u8(&ctx.soa.state, idx) & forbid_state) != 0 {
                        hoist_ok = false;
                        break;
                    }
                }
                if hoist_ok {
                    for t in fw..m {
                        if t == k || ctx.soa.is_deleted(t) {
                            continue;
                        }
                        if ctx.soa.is_branch_or_barrier(t) {
                            hoist_ok = false;
                            break;
                        }
                        let t_defs = at_u32(&ctx.soa.gpr_defs, t);
                        let t_uses = at_u32(&ctx.soa.gpr_uses, t);
                        if (m_uses & t_defs) != 0 || (m_defs & t_uses) != 0 || (m_defs & t_defs) != 0 {
                            hoist_ok = false;
                            break;
                        }
                    }
                }
                if !hoist_ok {
                    continue;
                }

                let new_cond = if csel_cond == 1 { orig_cc } else { orig_cc ^ 1 };
                let nw = (at_u32(&ctx.soa.words, m) & !(0xfu32 << 12)) | (u32::from(new_cond & 0xf) << 12);
                ctx.soa.replace_word(m, nw);
                ctx.soa.rotate_right_1(fw, m);
                // After rotating `[fw..=m]` right by 1, `k` (`fw < k < m`) shifted to `k + 1`.
                ctx.mark_deleted(k + 1);
                ctx.mark_deleted(i);
                stats.movs_eliminated += 2;
                changed = true;
                compute_mbb_live_after(&ctx.soa, mbb_start, mbb_end, live_out, live_after);
            }
            Some(_) => {}
        }
    }

    changed
}

/// Pass 1f: Redundant straight-line store-to-load reload elimination.
///
/// Within an MBB `mbb_start..mbb_end`, eliminates or replaces full-width reloads (`ldr`/`ldur`)
/// that read the exact `(rn, imm_bytes, size, is_fp)` slot stored by an earlier `str`/`stur`
/// with no intervening store, memory/control barrier, or modification to `rn` or `rt`:
/// - Same-register full-width reload (`X64` GPR or `Q128` FP): deleted (`stats.movs_eliminated += 1`).
/// - Cross-register 64-bit GPR reload (`X64`, `rt < 31`, `rt2 < 31`): replaced with `mov x_rt2, x_rt`.
/// - 64-bit scalar FP reload (`D64`, `is_fp && size == MemSize::X64`): replaced with `fmov d_rt2, d_rt`
///   to preserve exact AArch64 upper-64-bit zeroing semantics while removing the memory reload.
fn try_elim_store_load_reload(
    ctx: &mut MachContext<'_>,
    mbb_start: usize,
    mbb_end: usize,
    stats: &mut MachStats,
) {
    for i in mbb_start..mbb_end {
        if ctx.soa.is_deleted(i)
            || ctx.soa.is_pinned(i)
            || ctx.soa.is_opaque_data(i)
            || matches!(ctx.soa.mem_at(i), MemEffect::Barrier)
        {
            continue;
        }
        let DecodedKind::StrOff { rt, rn, imm_bytes, size, is_fp } = ctx.soa.kind_at(i) else {
            continue;
        };
        let eligible = if is_fp {
            matches!(size, MemSize::X64 | MemSize::Q128)
        } else {
            size == MemSize::X64 && rt < 31
        };
        if !eligible {
            continue;
        }

        let base_bit = gpr_bit_sp(rn);
        let rt_gpr_bit = if is_fp { 0 } else { gpr_bit_zr(rt) };
        let rt_fp_bit = if is_fp { fp_bit(rt) } else { 0 };
        let scan_end = mbb_end.min(i + 9);

        for j in (i + 1)..scan_end {
            if ctx.soa.is_deleted(j) {
                continue;
            }
            if ctx.soa.is_target_boundary(j) {
                break;
            }

            if !ctx.soa.is_pinned(j)
                && !ctx.soa.is_opaque_data(j)
                && !matches!(ctx.soa.mem_at(j), MemEffect::Barrier)
                && let DecodedKind::LdrOff { rt: rt2, rn: rn2, imm_bytes: off2, size: size2, is_fp: is_fp2 } =
                    ctx.soa.kind_at(j)
                && rn2 == rn
                && off2 == imm_bytes
                && size2 == size
                && is_fp2 == is_fp
            {
                if !is_fp {
                    if rt2 == rt {
                        if ctx.allow_delete {
                            ctx.mark_deleted(j);
                            stats.movs_eliminated += 1;
                            continue;
                        }
                    } else if rt2 < 31 {
                        ctx.soa.replace_word(j, enc::mov(u32::from(rt2), u32::from(rt)));
                        stats.movs_eliminated += 1;
                        if rt2 == rn {
                            break;
                        }
                        continue;
                    }
                } else if size == MemSize::Q128 {
                    if rt2 == rt && ctx.allow_delete {
                        ctx.mark_deleted(j);
                        stats.movs_eliminated += 1;
                        continue;
                    }
                } else {
                    ctx.soa.replace_word(j, enc::fmov_reg(true, u32::from(rt2), u32::from(rt)));
                    stats.movs_eliminated += 1;
                    continue;
                }
            }

            if matches!(ctx.soa.mem_at(j), MemEffect::Store | MemEffect::Barrier)
                || ctx.soa.is_branch_or_barrier(j)
                || ctx.soa.is_opaque_data(j)
            {
                break;
            }
            let j_gdefs = at_u32(&ctx.soa.gpr_defs, j);
            if (j_gdefs & base_bit) != 0 {
                break;
            }
            if is_fp {
                if (at_u32(&ctx.soa.fp_defs, j) & rt_fp_bit) != 0 {
                    break;
                }
            } else if (j_gdefs & rt_gpr_bit) != 0 {
                break;
            }
        }
    }
}

// =============================================================================
// 6b. Pass 1b — `mach_redundant_loads` (Intra-MBB Redundant Load & Store-to-Load Forwarding)
// =============================================================================

/// Maximum number of active straight-line memory facts tracked per sub-region in
/// [`mach_redundant_loads`] (stack-allocated, zero heap allocations).
const MAX_MEM_FACTS: usize = 16;

/// One proven straight-line equivalence: GPR `val_reg` (`< 31`) holds the value of
/// `[base + imm_bytes]` (width `size`).
#[derive(Clone, Copy)]
struct MemFact {
    base: u8,
    imm_bytes: i32,
    size: MemSize,
    val_reg: u8,
    /// True iff all 64 bits of `val_reg` equal the zero-extended value of the slot:
    /// - always `true` after unsigned `LdrOff` (`ldrb`/`ldrh`/`ldr w`/`ldr x`) or `X64` `StrOff`,
    /// - `false` after narrow `StrOff` (`B8`/`H16`/`W32`) whose upper bits in `val_reg` may be dirty.
    exact_zext: bool,
}

impl MemFact {
    const EMPTY: Self = Self {
        base: 0,
        imm_bytes: 0,
        size: MemSize::X64,
        val_reg: 0,
        exact_zext: false,
    };
}

/// True iff byte intervals `[off1, off1 + size1)` and `[off2, off2 + size2)` from the SAME
/// base register overlap (computed in `i64` to avoid signed overflow).
#[inline]
fn mem_ranges_overlap(off1: i32, size1: MemSize, off2: i32, size2: MemSize) -> bool {
    let a_start = i64::from(off1);
    let a_end = a_start + i64::from(size1.bytes());
    let b_start = i64::from(off2);
    let b_end = b_start + i64::from(size2.bytes());
    a_start < b_end && b_start < a_end
}

struct MemFactTable {
    items: [MemFact; MAX_MEM_FACTS],
    len: usize,
}

impl MemFactTable {
    #[inline]
    const fn new() -> Self {
        Self { items: [MemFact::EMPTY; MAX_MEM_FACTS], len: 0 }
    }

    #[inline]
    fn clear(&mut self) {
        self.len = 0;
    }

    #[inline]
    fn retain(&mut self, mut keep: impl FnMut(&MemFact) -> bool) {
        let mut dst = 0usize;
        for src in 0..self.len {
            if let Some(&f) = self.items.get(src)
                && keep(&f)
            {
                if dst != src {
                    set_at(&mut self.items, dst, f);
                }
                dst += 1;
            }
        }
        self.len = dst;
    }

    #[inline]
    fn invalidate_reg(&mut self, reg: u8) {
        self.retain(|f| f.base != reg && f.val_reg != reg);
    }

    #[inline]
    fn invalidate_defs(&mut self, defs: u32) {
        if defs != 0 {
            self.retain(|f| (defs & (gpr_bit_sp(f.base) | gpr_bit_zr(f.val_reg))) == 0);
        }
    }

    /// Sound aliasing invalidation for a store to `[rn + imm_bytes, +size)`:
    /// - different base register (`f.base != rn`) could alias -> invalidate,
    /// - same base register (`f.base == rn`) -> invalidate iff byte ranges overlap.
    #[inline]
    fn invalidate_store(&mut self, rn: u8, imm_bytes: i32, size: MemSize) {
        self.retain(|f| f.base == rn && !mem_ranges_overlap(f.imm_bytes, f.size, imm_bytes, size));
    }

    /// Finds the best matching active fact for `(base, imm_bytes, size)`, preferring exact
    /// same-register zero-extended matches (which permit complete deletion).
    fn find_match(&self, base: u8, imm_bytes: i32, size: MemSize, rt: u8) -> Option<MemFact> {
        let mut best: Option<MemFact> = None;
        let mut best_rank = 0u8;
        for i in 0..self.len {
            let Some(&f) = self.items.get(i) else {
                continue;
            };
            if f.base != base || f.imm_bytes != imm_bytes || f.size != size {
                continue;
            }
            let rank = if f.val_reg == rt && f.exact_zext {
                4
            } else if f.val_reg == rt {
                3
            } else if f.exact_zext {
                2
            } else {
                1
            };
            if rank >= best_rank {
                best_rank = rank;
                best = Some(f);
            }
        }
        best
    }

    fn insert(&mut self, fact: MemFact) {
        for i in 0..self.len {
            if let Some(slot) = self.items.get_mut(i)
                && slot.base == fact.base
                && slot.imm_bytes == fact.imm_bytes
                && slot.size == fact.size
                && slot.val_reg == fact.val_reg
            {
                slot.exact_zext = fact.exact_zext;
                return;
            }
        }
        if self.len < MAX_MEM_FACTS {
            set_at(&mut self.items, self.len, fact);
            self.len += 1;
        } else {
            self.items.copy_within(1..MAX_MEM_FACTS, 0);
            set_at(&mut self.items, MAX_MEM_FACTS - 1, fact);
        }
    }
}

/// Pass 1b: 100% sound intra-MBB redundant load elimination & store-to-load forwarding.
///
/// Tracks up to 16 active `[base + imm_bytes, size] == val_reg` facts per straight-line sub-region
/// (cleared at every `STATE_TARGET_BOUNDARY`, branch/barrier, `MemEffect::Barrier`, or opaque data):
/// - Same-register `exact_zext` reload -> deleted when `allow_delete` (or `nop` otherwise).
/// - `W32` reload -> 32-bit zero-extending `mov wt, wval` (0-cycle rename on Apple M5, sound even
///   after `str w9, [sp, #off]` when upper 32 bits of `x9` may be dirty).
/// - `X64` (and `exact_zext` `B8`/`H16`) cross-register reload -> 64-bit `mov xt, xval`.
/// - Runs immediately before Pass 2 (`mach_pair_ldp_stp`) so deleting duplicate base-pointer spill
///   reloads (`ldr x17, [sp]`) unlocks adjacent `ldp` pairing.
fn mach_redundant_loads(ctx: &mut MachContext<'_>, stats: &mut MachStats) {
    let n = ctx.soa.len();
    let mut facts = MemFactTable::new();

    for i in 0..n {
        if ctx.soa.is_deleted(i) {
            continue;
        }
        if ctx.soa.is_target_boundary(i) {
            facts.clear();
        }
        if ctx.soa.is_branch_or_barrier(i)
            || matches!(ctx.soa.mem_at(i), MemEffect::Barrier)
            || ctx.soa.is_opaque_data(i)
        {
            facts.clear();
            continue;
        }

        match ctx.soa.kind_at(i) {
            // Unsigned zero-extending GPR load (`ldr`/`ldur`/`ldrb`/`ldrh` — sign-extending
            // `ldrsw`/`ldrsb`/`ldrsh` decode to `DecodedKind::Other` and are never matched here).
            DecodedKind::LdrOff { rt, rn, imm_bytes, size, is_fp: false } => {
                if rt >= 31 {
                    continue;
                }
                if !ctx.soa.is_pinned(i)
                    && let Some(fact) = facts.find_match(rn, imm_bytes, size, rt)
                {
                    if fact.val_reg == rt && fact.exact_zext {
                        if ctx.allow_delete {
                            ctx.mark_deleted(i);
                            stats.movs_eliminated += 1;
                            continue;
                        }
                        ctx.soa.replace_word(i, enc::nop());
                        stats.movs_eliminated += 1;
                        continue;
                    }
                    if size == MemSize::W32 {
                        ctx.soa.replace_word(
                            i,
                            enc::sf(false, enc::mov(u32::from(rt), u32::from(fact.val_reg))),
                        );
                        stats.movs_eliminated += 1;
                        facts.invalidate_reg(rt);
                        if rn != rt {
                            facts.insert(MemFact {
                                base: rn,
                                imm_bytes,
                                size,
                                val_reg: rt,
                                exact_zext: true,
                            });
                        }
                        continue;
                    }
                    if fact.exact_zext && fact.val_reg != rt {
                        ctx.soa.replace_word(i, enc::mov(u32::from(rt), u32::from(fact.val_reg)));
                        stats.movs_eliminated += 1;
                        facts.invalidate_reg(rt);
                        if rn != rt {
                            facts.insert(MemFact {
                                base: rn,
                                imm_bytes,
                                size,
                                val_reg: rt,
                                exact_zext: true,
                            });
                        }
                        continue;
                    }
                }

                facts.invalidate_reg(rt);
                if !ctx.soa.is_pinned(i) && rn != rt {
                    facts.insert(MemFact {
                        base: rn,
                        imm_bytes,
                        size,
                        val_reg: rt,
                        exact_zext: true,
                    });
                }
            }
            // Non-writeback GPR or SIMD&FP store (`str`/`stur` — exact `[rn + imm_bytes, +size)`).
            DecodedKind::StrOff { rt, rn, imm_bytes, size, is_fp } => {
                facts.invalidate_store(rn, imm_bytes, size);
                // Guard `rt < 31`: encoding 31 in `StrOff` is `XZR`/`WZR`, never a general register.
                if !ctx.soa.is_pinned(i) && !is_fp && rt < 31 {
                    facts.insert(MemFact {
                        base: rn,
                        imm_bytes,
                        size,
                        val_reg: rt,
                        exact_zext: size == MemSize::X64,
                    });
                }
            }
            _ => {
                if matches!(ctx.soa.mem_at(i), MemEffect::Store) {
                    facts.clear();
                } else {
                    facts.invalidate_defs(at_u32(&ctx.soa.gpr_defs, i));
                }
            }
        }
    }
}

// =============================================================================
// 7. Pass 2 — `mach_pair_ldp_stp` (Cross-ALU Non-Aliasing LDP/STP Pairing)
// =============================================================================

fn encode_ldp_stp_pair(is_load: bool, is_fp: bool, size: MemSize, rt1: u8, rt2: u8, rn: u8, off: i32) -> Option<u32> {
    let stride = size.bytes();
    if stride < 4 || off % stride != 0 {
        return None;
    }
    let imm7 = off / stride;
    if !(-64..=63).contains(&imm7) {
        return None;
    }
    let r1 = u32::from(rt1);
    let r2 = u32::from(rt2);
    let base = u32::from(rn);
    match (is_load, is_fp, size) {
        (true, false, MemSize::W32) => Some(enc::ldp_w_off(r1, r2, base, off)),
        (true, false, MemSize::X64) => Some(enc::ldp_off(r1, r2, base, off)),
        (false, false, MemSize::W32) => Some(enc::stp_w_off(r1, r2, base, off)),
        (false, false, MemSize::X64) => Some(enc::stp_off(r1, r2, base, off)),
        (true, true, MemSize::W32) => Some(enc::ldp_s_off(r1, r2, base, off)),
        (true, true, MemSize::X64) => Some(enc::ldp_d_off(r1, r2, base, off)),
        (true, true, MemSize::Q128) => Some(enc::ldp_q_off(r1, r2, base, imm7)),
        (false, true, MemSize::W32) => Some(enc::stp_s_off(r1, r2, base, off)),
        (false, true, MemSize::X64) => Some(enc::stp_d_off(r1, r2, base, off)),
        (false, true, MemSize::Q128) => Some(enc::stp_q_off(r1, r2, base, imm7)),
        _ => None,
    }
}

fn mach_pair_ldp_stp(ctx: &mut MachContext<'_>, stats: &mut MachStats) {
    if !ctx.allow_delete {
        return;
    }
    let n = ctx.soa.len();
    let mut i = 0usize;
    while i < n {
        if ctx.soa.is_deleted(i)
            || ctx.soa.is_pinned(i)
            || ctx.soa.is_opaque_data(i)
            || ctx.soa.is_branch_or_barrier(i)
        {
            i += 1;
            continue;
        }
        let (is_load, rt_i, rn_i, off_i, size_i, is_fp_i) = match ctx.soa.kind_at(i) {
            DecodedKind::LdrOff { rt, rn, imm_bytes, size, is_fp } => (true, rt, rn, imm_bytes, size, is_fp),
            DecodedKind::StrOff { rt, rn, imm_bytes, size, is_fp } => (false, rt, rn, imm_bytes, size, is_fp),
            _ => {
                i += 1;
                continue;
            }
        };
        let stride = size_i.bytes();
        if stride < 4 || (!is_fp_i && matches!(size_i, MemSize::Q128)) {
            i += 1;
            continue;
        }
        // For GPR loads, if `i` itself overwrote `rn`, `j` would read a different base pointer!
        if is_load && !is_fp_i && rt_i == rn_i {
            i += 1;
            continue;
        }
        if is_load && !is_fp_i && rt_i == 31 {
            i += 1;
            continue;
        }

        let base_bit = gpr_bit_sp(rn_i);
        let max_j = n.min(i + PAIR_WINDOW + 1);
        let mut mid_gpr_defs = 0u32;
        let mut mid_gpr_uses = 0u32;
        let mut mid_fp_defs = 0u32;
        let mut mid_fp_uses = 0u32;

        for j in (i + 1)..max_j {
            if ctx.soa.is_deleted(j) {
                continue;
            }
            if ctx.soa.is_target_boundary(j)
                || ctx.soa.is_pinned(j)
                || ctx.soa.is_opaque_data(j)
                || ctx.soa.is_branch_or_barrier(j)
            {
                break;
            }

            // Check if `j` is a matching adjacent pair partner.
            let partner = match (is_load, ctx.soa.kind_at(j)) {
                (true, DecodedKind::LdrOff { rt, rn, imm_bytes, size, is_fp })
                    if rn == rn_i && size == size_i && is_fp == is_fp_i =>
                {
                    Some((rt, imm_bytes))
                }
                (false, DecodedKind::StrOff { rt, rn, imm_bytes, size, is_fp })
                    if rn == rn_i && size == size_i && is_fp == is_fp_i =>
                {
                    Some((rt, imm_bytes))
                }
                _ => None,
            };

            if let Some((rt_j, off_j)) = partner {
                let adj = if off_j == off_i + stride {
                    Some((rt_i, rt_j, off_i))
                } else if off_i == off_j + stride {
                    Some((rt_j, rt_i, off_j))
                } else {
                    None
                };
                if let Some((rt1, rt2, base_off)) = adj {
                    // AArch64 LDP forbids `rt1 == rt2`.
                    let legal_regs = !is_load || (rt1 != rt2 && (is_fp_i || rt2 < 31));
                    if legal_regs
                        && let Some(pair_word) =
                            encode_ldp_stp_pair(is_load, is_fp_i, size_i, rt1, rt2, rn_i, base_off)
                    {
                        let (r_i_touched_defs, r_i_touched_any, r_j_touched_defs, r_j_touched_any) = if is_fp_i {
                            let bi = fp_bit(rt_i);
                            let bj = fp_bit(rt_j);
                            (
                                (mid_fp_defs & bi) != 0,
                                ((mid_fp_defs | mid_fp_uses) & bi) != 0,
                                (mid_fp_defs & bj) != 0,
                                ((mid_fp_defs | mid_fp_uses) & bj) != 0,
                            )
                        } else {
                            let bi = gpr_bit_zr(rt_i);
                            let bj = gpr_bit_zr(rt_j);
                            (
                                (mid_gpr_defs & bi) != 0,
                                ((mid_gpr_defs | mid_gpr_uses) & bi) != 0,
                                (mid_gpr_defs & bj) != 0,
                                ((mid_gpr_defs | mid_gpr_uses) & bj) != 0,
                            )
                        };

                        // Prefer placing LDP at `i` (hoists load earlier!) and STP at `j` (sinks store!).
                        let placement = if is_load {
                            if !r_j_touched_any {
                                Some((i, j))
                            } else if !r_i_touched_any {
                                Some((j, i))
                            } else {
                                None
                            }
                        } else if !r_i_touched_defs {
                            Some((j, i))
                        } else if !r_j_touched_defs {
                            Some((i, j))
                        } else {
                            None
                        };

                        if let Some((keep_idx, del_idx)) = placement {
                            ctx.soa.replace_word(keep_idx, pair_word);
                            ctx.mark_deleted(del_idx);
                            stats.pairs_fused += 1;
                            break;
                        }
                    }
                }
            }

            // Update intervening-instruction hazards for `j`:
            // - Base register `rn_i` modified -> cannot cross!
            // - Memory hazard: loads cannot cross Store/Barrier; stores cannot cross Load/Store/Barrier!
            let j_gdefs = at_u32(&ctx.soa.gpr_defs, j);
            if (j_gdefs & base_bit) != 0 {
                break;
            }
            let j_mem = ctx.soa.mem_at(j);
            let mem_conflict = if is_load {
                matches!(j_mem, MemEffect::Store | MemEffect::Barrier)
            } else {
                !matches!(j_mem, MemEffect::None)
            };
            if mem_conflict {
                break;
            }
            mid_gpr_defs |= j_gdefs;
            mid_gpr_uses |= at_u32(&ctx.soa.gpr_uses, j);
            mid_fp_defs |= at_u32(&ctx.soa.fp_defs, j);
            mid_fp_uses |= at_u32(&ctx.soa.fp_uses, j);
        }

        i += 1;
    }
}

// =============================================================================
// 7b. Pass 2b — `mach_fuse_post_index` (Post-Index Load/Store Pointer-Update Fusion)
// =============================================================================

#[derive(Clone, Copy)]
enum PostIndexCand {
    LdpQ { rt1: u8, rt2: u8, rn: u8 },
    StpQ { rt1: u8, rt2: u8, rn: u8 },
    LdpX { rt1: u8, rt2: u8, rn: u8 },
    StpX { rt1: u8, rt2: u8, rn: u8 },
    LdpD { rt1: u8, rt2: u8, rn: u8 },
    StpD { rt1: u8, rt2: u8, rn: u8 },
    LdrQ { rt: u8, rn: u8 },
    StrQ { rt: u8, rn: u8 },
    LdrX { rt: u8, rn: u8 },
    StrX { rt: u8, rn: u8 },
    LdrD { rt: u8, rn: u8 },
    StrD { rt: u8, rn: u8 },
}

impl PostIndexCand {
    #[inline]
    const fn rn(self) -> u8 {
        match self {
            Self::LdpQ { rn, .. }
            | Self::StpQ { rn, .. }
            | Self::LdpX { rn, .. }
            | Self::StpX { rn, .. }
            | Self::LdpD { rn, .. }
            | Self::StpD { rn, .. }
            | Self::LdrQ { rn, .. }
            | Self::StrQ { rn, .. }
            | Self::LdrX { rn, .. }
            | Self::StrX { rn, .. }
            | Self::LdrD { rn, .. }
            | Self::StrD { rn, .. } => rn,
        }
    }

    #[inline]
    fn encode_post(self, delta: i32) -> Option<u32> {
        if delta == 0 {
            return None;
        }
        match self {
            Self::LdpQ { rt1, rt2, rn } => {
                if delta % 16 == 0 && (-1024..=1008).contains(&delta) {
                    Some(enc::ldp_q_post(u32::from(rt1), u32::from(rt2), u32::from(rn), delta / 16))
                } else {
                    None
                }
            }
            Self::StpQ { rt1, rt2, rn } => {
                if delta % 16 == 0 && (-1024..=1008).contains(&delta) {
                    Some(enc::stp_q_post(u32::from(rt1), u32::from(rt2), u32::from(rn), delta / 16))
                } else {
                    None
                }
            }
            Self::LdpX { rt1, rt2, rn } => {
                if delta % 8 == 0 && (-512..=504).contains(&delta) {
                    Some(enc::ldp_post(u32::from(rt1), u32::from(rt2), u32::from(rn), delta))
                } else {
                    None
                }
            }
            Self::StpX { rt1, rt2, rn } => {
                if delta % 8 == 0 && (-512..=504).contains(&delta) {
                    Some(enc::stp_post(u32::from(rt1), u32::from(rt2), u32::from(rn), delta))
                } else {
                    None
                }
            }
            Self::LdpD { rt1, rt2, rn } => {
                if delta % 8 == 0 && (-512..=504).contains(&delta) {
                    Some(enc::ldp_post_d(u32::from(rt1), u32::from(rt2), u32::from(rn), delta))
                } else {
                    None
                }
            }
            Self::StpD { rt1, rt2, rn } => {
                if delta % 8 == 0 && (-512..=504).contains(&delta) {
                    Some(enc::stp_post_d(u32::from(rt1), u32::from(rt2), u32::from(rn), delta))
                } else {
                    None
                }
            }
            Self::LdrQ { rt, rn } => {
                if (-256..=255).contains(&delta) {
                    Some(enc::ldr_q_post(u32::from(rt), u32::from(rn), delta))
                } else {
                    None
                }
            }
            Self::StrQ { rt, rn } => {
                if (-256..=255).contains(&delta) {
                    Some(enc::str_q_post(u32::from(rt), u32::from(rn), delta))
                } else {
                    None
                }
            }
            Self::LdrX { rt, rn } => {
                if (-256..=255).contains(&delta) {
                    Some(enc::ldr_post(u32::from(rt), u32::from(rn), delta))
                } else {
                    None
                }
            }
            Self::StrX { rt, rn } => {
                if (-256..=255).contains(&delta) {
                    Some(enc::str_post(u32::from(rt), u32::from(rn), delta))
                } else {
                    None
                }
            }
            Self::LdrD { rt, rn } => {
                if (-256..=255).contains(&delta) {
                    Some(enc::fp_access(enc::ldr_post(u32::from(rt), u32::from(rn), delta)))
                } else {
                    None
                }
            }
            Self::StrD { rt, rn } => {
                if (-256..=255).contains(&delta) {
                    Some(enc::fp_access(enc::str_post(u32::from(rt), u32::from(rn), delta)))
                } else {
                    None
                }
            }
        }
    }
}

fn match_post_index_cand(w: u32, kind: DecodedKind) -> Option<PostIndexCand> {
    match kind {
        DecodedKind::LdrOff { rt, rn, imm_bytes: 0, size, is_fp } if rn < 31 => match (size, is_fp) {
            (MemSize::Q128, true) => Some(PostIndexCand::LdrQ { rt, rn }),
            (MemSize::X64, false) if rt != rn => Some(PostIndexCand::LdrX { rt, rn }),
            (MemSize::X64, true) => Some(PostIndexCand::LdrD { rt, rn }),
            _ => None,
        },
        DecodedKind::StrOff { rt, rn, imm_bytes: 0, size, is_fp } if rn < 31 => match (size, is_fp) {
            (MemSize::Q128, true) => Some(PostIndexCand::StrQ { rt, rn }),
            (MemSize::X64, false) if rt != rn => Some(PostIndexCand::StrX { rt, rn }),
            (MemSize::X64, true) => Some(PostIndexCand::StrD { rt, rn }),
            _ => None,
        },
        DecodedKind::Other => {
            let rt1 = reg5(w);
            let rn = reg5(w >> 5);
            let rt2 = reg5(w >> 10);
            if rn >= 31 {
                return None;
            }
            // Zero-offset signed-offset `ldp`/`stp` (`bits 31:15` fixed, `imm7 == 0`).
            match w & 0xffff_8000 {
                0xad40_0000 if rt1 != rt2 => Some(PostIndexCand::LdpQ { rt1, rt2, rn }),
                0xad00_0000 => Some(PostIndexCand::StpQ { rt1, rt2, rn }),
                0xa940_0000 if rt1 != rt2 && rt1 != rn && rt2 != rn => Some(PostIndexCand::LdpX { rt1, rt2, rn }),
                0xa900_0000 if rt1 != rn && rt2 != rn => Some(PostIndexCand::StpX { rt1, rt2, rn }),
                0x6d40_0000 if rt1 != rt2 => Some(PostIndexCand::LdpD { rt1, rt2, rn }),
                0x6d00_0000 => Some(PostIndexCand::StpD { rt1, rt2, rn }),
                _ => None,
            }
        }
        _ => None,
    }
}

fn mach_fuse_post_index(ctx: &mut MachContext<'_>, stats: &mut MachStats) {
    if !ctx.allow_delete {
        return;
    }
    let n = ctx.soa.len();
    for i in 0..n {
        if ctx.soa.is_deleted(i)
            || ctx.soa.is_pinned(i)
            || ctx.soa.is_opaque_data(i)
            || ctx.soa.is_branch_or_barrier(i)
            || matches!(ctx.soa.mem_at(i), MemEffect::Barrier)
        {
            continue;
        }
        let Some(cand) = match_post_index_cand(at_u32(&ctx.soa.words, i), ctx.soa.kind_at(i)) else {
            continue;
        };
        let rn = cand.rn();
        let rn_bit = 1u32 << rn;
        let scan_end = n.min(i + POST_INDEX_WINDOW);

        for j in (i + 1)..scan_end {
            if ctx.soa.is_deleted(j) {
                continue;
            }
            if ctx.soa.is_target_boundary(j)
                || ctx.soa.is_pinned(j)
                || ctx.soa.is_opaque_data(j)
                || ctx.soa.is_branch_or_barrier(j)
                || matches!(ctx.soa.mem_at(j), MemEffect::Barrier)
            {
                break;
            }

            if let DecodedKind::AddSubImm {
                rd,
                rn: add_rn,
                imm12,
                shift12: false,
                is_sub,
                wide: true,
                sets_flags: false,
            } = ctx.soa.kind_at(j)
                && rd == rn
                && add_rn == rn
            {
                let delta = if is_sub {
                    -i32::from(imm12)
                } else {
                    i32::from(imm12)
                };
                if let Some(new_w) = cand.encode_post(delta) {
                    ctx.soa.replace_word(i, new_w);
                    ctx.mark_deleted(j);
                    stats.pairs_fused += 1;
                }
                break;
            }

            // Stop if any intervening instruction reads or writes `rn` so mutating `rn` at `i`
            // is never observed before `j`.
            if ((at_u32(&ctx.soa.gpr_uses, j) | at_u32(&ctx.soa.gpr_defs, j)) & rn_bit) != 0 {
                break;
            }
        }
    }
}

// =============================================================================
// 8. Pass 3 — `mach_sched_m5` (Post-RA Apple M5 / Graviton 5 List Scheduler)
// =============================================================================

/// Apple M5 per-cycle issue pipe limits: `[Alu: 6, LdSt: 3, Fp: 4, Branch: 1]`.
const M5_PIPE_CAPS: [u8; 4] = [6, 3, 4, 1];
/// Apple M5 total decode/issue width per cycle.
const M5_ISSUE_WIDTH: u8 = 8;

fn mach_sched_m5(ctx: &mut MachContext<'_>, stats: &mut MachStats) {
    let n = ctx.soa.len();
    let mut pos = 0usize;
    while pos < n {
        // Find maximal straight-line non-barrier, non-pinned, non-opaque region `[r_start .. r_end)`.
        if !sched_eligible(&ctx.soa, pos) {
            pos += 1;
            continue;
        }
        let r_start = pos;
        pos += 1;
        while pos < n && !ctx.soa.is_target_boundary(pos) && sched_eligible(&ctx.soa, pos) {
            pos += 1;
        }
        let r_end = pos;
        let len = r_end - r_start;
        if !(SCHED_MIN_LEN..=SCHED_MAX_LEN).contains(&len) {
            continue;
        }

        // Check Apple M5 macro-op fusion lock: does the instruction immediately after `r_end`
        // consume NZCV via `b.cc`?
        let followed_by_flag_branch = r_end < n
            && matches!(ctx.soa.kind_at(r_end), DecodedKind::BranchCond { is_flag_cond: true, .. });

        schedule_region_m5(ctx, r_start, r_end, followed_by_flag_branch, stats);
    }
}

#[inline]
fn sched_eligible(soa: &MachFuncSoa<'_>, idx: usize) -> bool {
    !soa.is_deleted(idx)
        && !soa.is_pinned(idx)
        && !soa.is_opaque_data(idx)
        && !soa.is_branch_or_barrier(idx)
        && !matches!(soa.mem_at(idx), MemEffect::Barrier)
}

fn schedule_region_m5(
    ctx: &mut MachContext<'_>,
    r_start: usize,
    r_end: usize,
    followed_by_flag_branch: bool,
    stats: &mut MachStats,
) {
    let len = r_end - r_start;
    debug_assert!(len <= SCHED_MAX_LEN);

    let mut succs = [0u128; SCHED_MAX_LEN];
    let mut is_raw = [0u128; SCHED_MAX_LEN];
    let mut pred_rem = [0u8; SCHED_MAX_LEN];
    let mut lat = [0u8; SCHED_MAX_LEN];
    let mut pipes = [M5Pipe::Alu; SCHED_MAX_LEN];
    let mut last_flag_writer: Option<usize> = None;

    for a in 0..len {
        let ga = r_start + a;
        set_at(&mut lat, a, at_u8(&ctx.soa.latency, ga));
        set_at(&mut pipes, a, ctx.soa.pipe_at(ga));
        if ctx.soa.writes_flags(ga) {
            last_flag_writer = Some(a);
        }
    }

    // Build exact intra-region RAW / WAR / WAW + memory dependency DAG.
    for a in 0..len {
        let ga = r_start + a;
        let a_gd = at_u32(&ctx.soa.gpr_defs, ga);
        let a_gu = at_u32(&ctx.soa.gpr_uses, ga);
        let a_fd = at_u32(&ctx.soa.fp_defs, ga);
        let a_fu = at_u32(&ctx.soa.fp_uses, ga);
        let a_rf = ctx.soa.reads_flags(ga);
        let a_wf = ctx.soa.writes_flags(ga);
        let a_mem = ctx.soa.mem_at(ga);

        for b in (a + 1)..len {
            let gb = r_start + b;
            let b_gd = at_u32(&ctx.soa.gpr_defs, gb);
            let b_gu = at_u32(&ctx.soa.gpr_uses, gb);
            let b_fd = at_u32(&ctx.soa.fp_defs, gb);
            let b_fu = at_u32(&ctx.soa.fp_uses, gb);
            let b_rf = ctx.soa.reads_flags(gb);
            let b_wf = ctx.soa.writes_flags(gb);
            let b_mem = ctx.soa.mem_at(gb);

            let raw = (a_gd & b_gu) != 0 || (a_fd & b_fu) != 0 || (a_wf && b_rf);
            let war = (a_gu & b_gd) != 0 || (a_fu & b_fd) != 0 || (a_rf && b_wf);
            let waw = (a_gd & b_gd) != 0 || (a_fd & b_fd) != 0 || (a_wf && b_wf);
            let mem_dep = !matches!(
                (a_mem, b_mem),
                (MemEffect::None, _) | (_, MemEffect::None) | (MemEffect::Load, MemEffect::Load)
            );

            if raw || war || waw || mem_dep {
                let b_bit = 1u128 << b;
                if let Some(s) = succs.get_mut(a) {
                    *s |= b_bit;
                }
                if raw && let Some(r) = is_raw.get_mut(a) {
                    *r |= b_bit;
                }
                if let Some(p) = pred_rem.get_mut(b) {
                    *p = p.saturating_add(1);
                }
            }
        }
    }

    // Apple M5 macro-op fusion lock: if the region is immediately followed by `b.cc` and the
    // last flag writer `f` has NO successors in this region (e.g. `cmp`/`cmn`/`tst`/`adds`/`subs`),
    // add ordering edges `u -> f` for all `u != f` so `f` is ALWAYS scheduled at `r_end - 1`!
    if followed_by_flag_branch
        && let Some(f) = last_flag_writer
        && succs.get(f).copied().unwrap_or(1) == 0
    {
        let f_bit = 1u128 << f;
        for u in 0..len {
            if u == f {
                continue;
            }
            let already = (succs.get(u).copied().unwrap_or(0) & f_bit) != 0;
            if !already {
                if let Some(s) = succs.get_mut(u) {
                    *s |= f_bit;
                }
                if let Some(p) = pred_rem.get_mut(f) {
                    *p = p.saturating_add(1);
                }
            }
        }
    }

    // Compute critical-path height backwards (`latency + max_succ_height`).
    let mut height = [0u16; SCHED_MAX_LEN];
    for a in (0..len).rev() {
        let a_lat = u16::from(lat.get(a).copied().unwrap_or(1));
        let mut h = a_lat;
        let mut mask = succs.get(a).copied().unwrap_or(0);
        let raw_mask = is_raw.get(a).copied().unwrap_or(0);
        while mask != 0 {
            let b = to_idx(mask.trailing_zeros());
            mask &= mask - 1;
            let edge_w = if ((raw_mask >> b) & 1) != 0 { a_lat } else { 0 };
            let bh = height.get(b).copied().unwrap_or(0);
            let cand = edge_w.saturating_add(bh);
            if cand > h {
                h = cand;
            }
        }
        set_at(&mut height, a, h);
    }

    // Critical-path list schedule with Apple M5 pipe balance.
    let mut ready = 0u128;
    for u in 0..len {
        if pred_rem.get(u).copied().unwrap_or(1) == 0 {
            ready |= 1u128 << u;
        }
    }
    let mut ready_at = [0u16; SCHED_MAX_LEN];
    let mut order = [0usize; SCHED_MAX_LEN];
    let mut cycle = 0u16;
    let mut pipe_used = [0u8; 4];
    let mut issued_in_cycle = 0u8;

    for step in 0..len {
        if ready == 0 {
            // Defensive: DAG must be acyclic; abort region schedule if somehow empty.
            return;
        }
        if issued_in_cycle >= M5_ISSUE_WIDTH {
            cycle = cycle.saturating_add(1);
            pipe_used = [0; 4];
            issued_in_cycle = 0;
        }

        // Advance cycle if no ready instruction is available yet.
        let mut min_ready = u16::MAX;
        let mut scan = ready;
        while scan != 0 {
            let u = to_idx(scan.trailing_zeros());
            scan &= scan - 1;
            let ra = ready_at.get(u).copied().unwrap_or(0);
            if ra < min_ready {
                min_ready = ra;
            }
        }
        if min_ready > cycle {
            cycle = min_ready;
            pipe_used = [0; 4];
            issued_in_cycle = 0;
        }

        // Pick best ready instruction deterministically:
        // 1. ready_at <= cycle
        // 2. pipe has spare capacity in current cycle
        // 3. highest critical-path height (hoists 4-cycle loads & 3-cycle multiplies early!)
        // 4. least-loaded pipe (balances M5 6 ALU / 3 LDST / 4 FP)
        // 5. lowest original program order index (deterministic tie-break)
        let mut best_u = usize::MAX;
        let mut best_key: (bool, bool, u16, u8, usize) = (false, false, 0, 0, usize::MAX);
        let mut cand_mask = ready;
        while cand_mask != 0 {
            let u = to_idx(cand_mask.trailing_zeros());
            cand_mask &= cand_mask - 1;
            let is_ready_now = ready_at.get(u).copied().unwrap_or(0) <= cycle;
            let p_idx = pipes.get(u).copied().unwrap_or(M5Pipe::Alu).index();
            let used = pipe_used.get(p_idx).copied().unwrap_or(0);
            let cap = M5_PIPE_CAPS.get(p_idx).copied().unwrap_or(1);
            let pipe_free = used < cap;
            let h = height.get(u).copied().unwrap_or(0);
            let key = (is_ready_now, pipe_free, h, u8::MAX - used, usize::MAX - u);
            if best_u == usize::MAX || key > best_key {
                best_u = u;
                best_key = key;
            }
        }

        set_at(&mut order, step, best_u);
        ready &= !(1u128 << best_u);

        let p_idx = pipes.get(best_u).copied().unwrap_or(M5Pipe::Alu).index();
        if let Some(pu) = pipe_used.get_mut(p_idx) {
            *pu = pu.saturating_add(1);
        }
        issued_in_cycle = issued_in_cycle.saturating_add(1);

        let u_lat = u16::from(lat.get(best_u).copied().unwrap_or(1));
        let mut smask = succs.get(best_u).copied().unwrap_or(0);
        let raw_mask = is_raw.get(best_u).copied().unwrap_or(0);
        while smask != 0 {
            let b = to_idx(smask.trailing_zeros());
            smask &= smask - 1;
            let edge_w = if ((raw_mask >> b) & 1) != 0 { u_lat } else { 0 };
            let avail = cycle.saturating_add(edge_w);
            if let Some(rb) = ready_at.get_mut(b)
                && avail > *rb
            {
                *rb = avail;
            }
            if let Some(pr) = pred_rem.get_mut(b) {
                *pr = pr.saturating_sub(1);
                if *pr == 0 {
                    ready |= 1u128 << b;
                }
            }
        }
    }

    // Count moved instructions and commit permutation only if the schedule changed.
    let mut moved = 0u32;
    for step in 0..len {
        if order.get(step).copied().unwrap_or(step) != step {
            moved += 1;
        }
    }
    if moved == 0 {
        return;
    }

    let mut saved_words = [0u32; SCHED_MAX_LEN];
    for i in 0..len {
        set_at(&mut saved_words, i, at_u32(&ctx.soa.words, r_start + i));
    }
    for step in 0..len {
        let orig = order.get(step).copied().unwrap_or(step);
        let w = saved_words.get(orig).copied().unwrap_or(0);
        ctx.soa.replace_word(r_start + step, w);
    }
    stats.insts_scheduled += moved;
}

// =============================================================================
// 9. Pass 4 — `mach_branch_opt` (Trampoline Tail-Duplication + Fallthrough Elision)
// =============================================================================

/// Returns the PC-relative displacement [`Field`] of an AArch64 branch word (`b`, `b.cond`,
/// `cbz`/`cbnz`, or `tbz`/`tbnz`).
#[inline]
const fn branch_field(w: u32) -> Option<Field> {
    if (w & 0xfc00_0000) == 0x1400_0000 {
        Some(Field::Imm26)
    } else if (w & 0xff00_0010) == 0x5400_0000 || (w & 0x7e00_0000) == 0x3400_0000 {
        Some(Field::Imm19)
    } else if (w & 0x7e00_0000) == 0x3600_0000 {
        Some(Field::Imm14)
    } else {
        None
    }
}

/// Decodes `movz xd/wd, #imm16, lsl #0` (`hw == 0`, `rd < 31`), returning `Some((rd, imm16))`.
#[inline]
const fn decode_movz_hw0(w: u32) -> Option<(u8, u32)> {
    // `movz`: `sf 10 100101 hw(00) imm16 Rd`.
    if (w & 0x7fe0_0000) != 0x5280_0000 {
        return None;
    }
    let rd = reg5(w);
    if rd >= 31 {
        return None;
    }
    Some((rd, (w >> 5) & 0xffff))
}

/// Tail-duplicates tiny trampoline regions `T` (`1..=6` non-NOP instructions ending in an
/// unconditional `b final_target` with `final_target != T`, no calls/indirect branches/switch
/// tables/relocs/traps/internal boundaries) into small predecessor MBBs `P` (`<= 4` non-NOP
/// instructions ending in a non-fallthrough `b T`), scanning BOTH LIR `block_word_off` blocks
/// AND trailing/pre-block `cold_stubs`, capped at `<= 64` duplicated instructions per function.
///
/// After duplication into `P`, folds any newly adjacent `movz xd, #imm16, lsl #0; lsr xd2, xd, #sh`
/// into `movz xd2, #(imm16 >> sh), lsl #0` (deleting the `lsr` when `xd2 == xd`).
/// Any duplicated trailing `b final_target` that now jumps to the immediately following word is
/// then automatically elided by the subsequent fallthrough sweep in [`mach_branch_opt`].
fn mach_tail_dup_trampolines<'a>(ctx: &mut MachContext<'a>, stats: &mut MachStats) {
    if !ctx.allow_delete || ctx.dirty_deletes {
        return;
    }
    let old_len = ctx.soa.len();
    if old_len < 2 {
        return;
    }

    let has_patch_targets = ctx.patch_target_blocks.len() == ctx.patch_word_idxs.len();
    let mut word_to_patch = vec_filled_in(old_len, usize::MAX, ctx.alloc);
    for (p, &widx) in ctx.patch_word_idxs.iter().enumerate() {
        if widx != ELIDED_PATCH && widx < old_len {
            set_at(&mut word_to_patch, widx, p);
        }
    }
    let mut word_to_internal = vec_filled_in(old_len, usize::MAX, ctx.alloc);
    for (ib, br) in ctx.internal_branches.iter().enumerate() {
        if br.src_word < old_len {
            set_at(&mut word_to_internal, br.src_word, ib);
        }
    }
    let mut is_extra_word = vec_filled_in(old_len, false, ctx.alloc);
    for &eidx in &ctx.extra_word_idxs {
        if eidx < old_len {
            set_at(&mut is_extra_word, eidx, true);
        }
    }

    // Partition ALL emitted words (both LIR `block_word_off` blocks AND trailing/pre-block
    // `cold_stubs`) into single-entry regions delimited by `STATE_TARGET_BOUNDARY` and
    // unconditional terminators (`b`, `ret`, `br`, `brk`). We intentionally do NOT split at
    // `BranchCond` here so tiny trampolines `T` containing an inline guard (`b.hi exit`) stay
    // a single region `[t_start..t_end)`.
    let mut is_reg_start = vec_filled_in(old_len, false, ctx.alloc);
    set_at(&mut is_reg_start, 0, true);
    for &off in &ctx.block_word_off {
        if off < old_len {
            set_at(&mut is_reg_start, off, true);
        }
    }
    for i in 0..old_len {
        if ctx.soa.is_target_boundary(i) {
            set_at(&mut is_reg_start, i, true);
        }
        let kd = ctx.soa.kind_at(i);
        if i + 1 < old_len
            && (matches!(
                kd,
                DecodedKind::BranchUncond { .. }
                    | DecodedKind::Ret
                    | DecodedKind::IndirectBranch
            ) || enc::is_unconditional_terminator(at_u32(&ctx.soa.words, i)))
        {
            set_at(&mut is_reg_start, i + 1, true);
        }
    }

    let mut reg_start: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(ctx.block_order.len() + 8, ctx.alloc);
    let mut reg_end: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(ctx.block_order.len() + 8, ctx.alloc);
    let mut word_to_reg = vec_filled_in(old_len + 1, usize::MAX, ctx.alloc);
    let mut cur = 0usize;
    while cur < old_len {
        let r = reg_start.len();
        let s = cur;
        cur += 1;
        while cur < old_len && !is_reg_start.get(cur).copied().unwrap_or(false) {
            cur += 1;
        }
        reg_start.push(s);
        reg_end.push(cur);
        set_at(&mut word_to_reg, s, r);
    }
    let num_regs = reg_start.len();
    if num_regs < 2 {
        return;
    }

    let num_blocks = ctx.block_word_off.len();
    let resolve_branch_dst = |widx: usize| -> Option<(usize, usize)> {
        let pidx = at_usize(&word_to_patch, widx);
        if pidx != usize::MAX {
            if !has_patch_targets {
                return None;
            }
            let tb = to_idx(at_u32(&ctx.patch_target_blocks, pidx));
            if tb >= num_blocks {
                return None;
            }
            let dst_w = at_usize(&ctx.block_word_off, tb);
            if dst_w > old_len {
                return None;
            }
            return Some((dst_w, at_usize(&word_to_reg, dst_w)));
        }
        if ctx.soa.is_pinned(widx) {
            return None;
        }
        let ib = at_usize(&word_to_internal, widx);
        if ib != usize::MAX {
            let br = ctx.internal_branches.get(ib)?;
            if br.dst_word > old_len {
                return None;
            }
            return Some((br.dst_word, at_usize(&word_to_reg, br.dst_word)));
        }
        None
    };

    // 1. Identify eligible tiny trampoline regions `T` (`1..=6` non-NOP instructions).
    let mut tramp_non_nop = vec_filled_in(num_regs, 0usize, ctx.alloc);
    for t_reg in 0..num_regs {
        let t_start = at_usize(&reg_start, t_reg);
        let t_end = at_usize(&reg_end, t_reg);
        if t_start >= t_end || t_end > old_len {
            continue;
        }
        let t_last = t_end - 1;
        if ctx.soa.is_deleted(t_last)
            || ctx.soa.is_reloc_or_trap(t_last)
            || ctx.soa.is_opaque_data(t_last)
            || is_extra_word.get(t_last).copied().unwrap_or(false)
            || !matches!(ctx.soa.kind_at(t_last), DecodedKind::BranchUncond { .. })
            || branch_field(at_u32(&ctx.soa.words, t_last)) != Some(Field::Imm26)
        {
            continue;
        }
        let Some((final_dst_w, final_reg)) = resolve_branch_dst(t_last) else {
            continue;
        };
        if final_reg == t_reg || (t_start..t_end).contains(&final_dst_w) {
            continue;
        }

        let mut non_nop = 0usize;
        let mut valid = true;
        for k in t_start..t_end {
            if ctx.soa.is_deleted(k)
                || ctx.soa.is_reloc_or_trap(k)
                || ctx.soa.is_opaque_data(k)
                || is_extra_word.get(k).copied().unwrap_or(false)
                || (k > t_start && ctx.soa.is_target_boundary(k))
            {
                valid = false;
                break;
            }
            if k == t_last {
                non_nop += 1;
                continue;
            }
            if matches!(ctx.soa.kind_at(k), DecodedKind::Nop) {
                continue;
            }
            non_nop += 1;
            if non_nop > TAIL_DUP_MAX_TRAMP_INSTS {
                valid = false;
                break;
            }
            if ctx.soa.is_branch_or_barrier(k) {
                if !matches!(ctx.soa.kind_at(k), DecodedKind::BranchCond { .. })
                    || branch_field(at_u32(&ctx.soa.words, k)).is_none()
                {
                    valid = false;
                    break;
                }
                let Some((dst_w, dst_reg)) = resolve_branch_dst(k) else {
                    valid = false;
                    break;
                };
                if dst_reg == t_reg || (t_start..t_end).contains(&dst_w) {
                    valid = false;
                    break;
                }
            } else if ctx.soa.is_pinned(k) {
                valid = false;
                break;
            }
        }
        if valid && (1..=TAIL_DUP_MAX_TRAMP_INSTS).contains(&non_nop) {
            set_at(&mut tramp_non_nop, t_reg, non_nop);
        }
    }

    // 2. Select small predecessor MBBs `P` (`<= 4` non-NOP instructions, covering both LIR blocks
    //    and cold stubs) jumping to a trampoline `T`.
    let mut dup_target_for_word = vec_filled_in(old_len, usize::MAX, ctx.alloc);
    let mut dup_p_ranges: Vec<(usize, usize), Alloc<'a>> = Vec::with_capacity_in(8, ctx.alloc);
    let mut total_dup_insts = 0usize;
    for p_reg in 0..num_regs {
        let p_start = at_usize(&reg_start, p_reg);
        let p_end = at_usize(&reg_end, p_reg);
        if p_start >= p_end || p_end > old_len {
            continue;
        }
        let p_last = p_end - 1;
        // Measure predecessor size over the trailing straight-line MBB ending at `p_last`
        // (after any earlier `BranchCond` inside the same region).
        let mut p_mbb_start = p_start;
        for k in (p_start..p_last).rev() {
            if !ctx.soa.is_deleted(k) && matches!(ctx.soa.kind_at(k), DecodedKind::BranchCond { .. }) {
                p_mbb_start = k + 1;
                break;
            }
        }
        let mut p_non_nop = 0usize;
        for k in p_mbb_start..p_end {
            if !ctx.soa.is_deleted(k) && !matches!(ctx.soa.kind_at(k), DecodedKind::Nop) {
                p_non_nop += 1;
            }
        }
        if p_non_nop == 0 || p_non_nop > TAIL_DUP_MAX_PRED_INSTS {
            continue;
        }
        if ctx.soa.is_deleted(p_last)
            || ctx.soa.is_reloc_or_trap(p_last)
            || ctx.soa.is_opaque_data(p_last)
            || is_extra_word.get(p_last).copied().unwrap_or(false)
            || !matches!(ctx.soa.kind_at(p_last), DecodedKind::BranchUncond { .. })
        {
            continue;
        }
        let Some((t_dst_w, t_reg)) = resolve_branch_dst(p_last) else {
            continue;
        };
        if t_reg == usize::MAX || t_reg == p_reg || t_reg == p_reg + 1 || t_dst_w == p_end {
            continue;
        }
        if t_dst_w != at_usize(&reg_start, t_reg) {
            continue;
        }
        let t_cost = at_usize(&tramp_non_nop, t_reg);
        if t_cost == 0 || total_dup_insts + t_cost > TAIL_DUP_MAX_TOTAL_INSTS {
            continue;
        }
        set_at(&mut dup_target_for_word, p_last, t_reg);
        dup_p_ranges.push((p_mbb_start, p_end));
        total_dup_insts += t_cost;
    }

    if dup_p_ranges.is_empty() {
        return;
    }

    // 3. Build expanded word stream, `old_to_new` map, and duplicated branch records.
    let cap = old_len.saturating_add(total_dup_insts);
    let mut new_words: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(cap, ctx.alloc);
    let mut new_state: Vec<u8, Alloc<'a>> = Vec::with_capacity_in(cap, ctx.alloc);
    let mut old_to_new: Vec<usize, Alloc<'a>> = Vec::with_capacity_in(old_len + 1, ctx.alloc);
    let mut pending_dup_branches: Vec<(usize, usize, Field), Alloc<'a>> =
        Vec::with_capacity_in(dup_p_ranges.len() * 2, ctx.alloc);

    let keep_mask = STATE_PINNED | STATE_RELOC_OR_TRAP | STATE_TARGET_BOUNDARY | STATE_OPAQUE_DATA;
    for i in 0..old_len {
        old_to_new.push(new_words.len());
        let t_reg = at_usize(&dup_target_for_word, i);
        if t_reg == usize::MAX {
            new_words.push(at_u32(&ctx.soa.words, i));
            new_state.push(at_u8(&ctx.soa.state, i) & keep_mask);
        } else {
            let t_start = at_usize(&reg_start, t_reg);
            let t_end = at_usize(&reg_end, t_reg);
            let mut first_dup = true;
            let entry_boundary = at_u8(&ctx.soa.state, i) & STATE_TARGET_BOUNDARY;
            for k in t_start..t_end {
                if matches!(ctx.soa.kind_at(k), DecodedKind::Nop) {
                    continue;
                }
                let new_idx = new_words.len();
                let w = at_u32(&ctx.soa.words, k);
                new_words.push(w);
                new_state.push(if first_dup {
                    first_dup = false;
                    entry_boundary
                } else {
                    0
                });
                if matches!(ctx.soa.kind_at(k), DecodedKind::BranchCond { .. } | DecodedKind::BranchUncond { .. }) {
                    let Some((orig_dst_w, _)) = resolve_branch_dst(k) else {
                        return;
                    };
                    let Some(field) = branch_field(w) else {
                        return;
                    };
                    pending_dup_branches.push((new_idx, orig_dst_w, field));
                }
            }
        }
    }
    old_to_new.push(new_words.len());

    // 4. Pre-verify & patch ALL internal branches (existing + duplicated) and verify all
    //    surviving `BranchPatch` displacements fit before committing.
    let mut new_internal_branches: Vec<InternalBranch, Alloc<'a>> =
        Vec::with_capacity_in(ctx.internal_branches.len() + pending_dup_branches.len(), ctx.alloc);
    for br in &ctx.internal_branches {
        if br.src_word >= old_len || br.dst_word > old_len {
            return;
        }
        if at_usize(&dup_target_for_word, br.src_word) != usize::MAX {
            continue;
        }
        new_internal_branches.push(InternalBranch {
            src_word: at_usize(&old_to_new, br.src_word),
            dst_word: at_usize(&old_to_new, br.dst_word),
            field: br.field,
        });
    }
    for &(new_src, orig_dst_w, field) in &pending_dup_branches {
        if orig_dst_w > old_len {
            return;
        }
        new_internal_branches.push(InternalBranch {
            src_word: new_src,
            dst_word: at_usize(&old_to_new, orig_dst_w),
            field,
        });
    }
    for br in &new_internal_branches {
        let (Some(src_i), Some(dst_i)) = (i64_of(br.src_word), i64_of(br.dst_word)) else {
            return;
        };
        let bytes = (dst_i - src_i) * 4;
        let Some(patched) = br.field.patch(at_u32(&new_words, br.src_word), bytes) else {
            return;
        };
        set_at(&mut new_words, br.src_word, patched);
    }
    if has_patch_targets {
        for (p, &old_w) in ctx.patch_word_idxs.iter().enumerate() {
            if old_w == ELIDED_PATCH
                || old_w >= old_len
                || at_usize(&dup_target_for_word, old_w) != usize::MAX
            {
                continue;
            }
            let tb = to_idx(at_u32(&ctx.patch_target_blocks, p));
            if tb < num_blocks {
                let old_dst = at_usize(&ctx.block_word_off, tb);
                if old_dst <= old_len {
                    let new_src = at_usize(&old_to_new, old_w);
                    let new_dst = at_usize(&old_to_new, old_dst);
                    if let Some(field) = branch_field(at_u32(&new_words, new_src)) {
                        let (Some(src_i), Some(dst_i)) = (i64_of(new_src), i64_of(new_dst)) else {
                            return;
                        };
                        if field.patch(at_u32(&new_words, new_src), (dst_i - src_i) * 4).is_none() {
                            return;
                        }
                    }
                }
            }
        }
    }

    // 5. Commit rebuilt SoA table and remap every offset table in lockstep.
    let new_len = new_words.len();
    let mut new_soa = MachFuncSoa::from_words(&new_words, ctx.ret_live_gprs, ctx.alloc);
    for (idx, &st) in new_state.iter().enumerate() {
        new_soa.add_state(idx, st);
    }
    ctx.soa = new_soa;

    for slot in ctx.block_word_off.iter_mut() {
        if *slot <= old_len {
            *slot = at_usize(&old_to_new, *slot);
        }
    }
    for slot in ctx.patch_word_idxs.iter_mut() {
        if *slot != ELIDED_PATCH && *slot < old_len {
            if at_usize(&dup_target_for_word, *slot) != usize::MAX {
                *slot = ELIDED_PATCH;
            } else {
                *slot = at_usize(&old_to_new, *slot);
            }
        }
    }
    for slot in ctx.reloc_byte_offs.iter_mut() {
        let old_w = to_idx(*slot) / 4;
        let rem = *slot % 4;
        if old_w <= old_len {
            *slot = u32_of(at_usize(&old_to_new, old_w) * 4) + rem;
        }
    }
    for slot in ctx.trap_byte_offs.iter_mut() {
        let old_w = to_idx(*slot) / 4;
        let rem = *slot % 4;
        if old_w <= old_len {
            *slot = u32_of(at_usize(&old_to_new, old_w) * 4) + rem;
        }
    }
    for slot in ctx.extra_word_idxs.iter_mut() {
        if *slot <= old_len {
            *slot = at_usize(&old_to_new, *slot);
        }
    }
    for slot in ctx.pinned_targets.iter_mut() {
        if *slot <= old_len {
            *slot = at_usize(&old_to_new, *slot);
        }
    }
    ctx.internal_branches = new_internal_branches;

    for &off in &ctx.block_word_off {
        if off < new_len {
            ctx.soa.add_state(off, STATE_TARGET_BOUNDARY);
        }
    }
    for &off in &ctx.pinned_targets {
        if off < new_len {
            ctx.soa.add_state(off, STATE_TARGET_BOUNDARY);
        }
    }
    for br in &ctx.internal_branches {
        if br.dst_word < new_len {
            ctx.soa.add_state(br.dst_word, STATE_TARGET_BOUNDARY);
        }
    }
    stats.branches_elided += u32_of(dup_p_ranges.len());

    // 6. Post-duplication constant-shift fold in each duplicated predecessor `P` (both LIR blocks
    //    and cold stubs): `movz xd, #imm16, lsl #0; lsr xd2, xd, #sh` -> `movz xd2, #(imm16 >> sh), lsl #0`.
    for &(old_p_start, old_p_end) in &dup_p_ranges {
        let p_start = at_usize(&old_to_new, old_p_start);
        let p_end = at_usize(&old_to_new, old_p_end);
        if p_end <= p_start + 1 || p_end > new_len {
            continue;
        }
        for i in p_start..(p_end - 1) {
            if ctx.soa.is_deleted(i)
                || ctx.soa.is_deleted(i + 1)
                || ctx.soa.is_opaque_data(i)
                || ctx.soa.is_opaque_data(i + 1)
                || ctx.soa.is_target_boundary(i + 1)
                || ctx.soa.is_pinned(i + 1)
            {
                continue;
            }
            let Some((rd_mov, imm16)) = decode_movz_hw0(at_u32(&ctx.soa.words, i)) else {
                continue;
            };
            let Some((lsr_wide, rd_lsr, rn_lsr, sh, lsr_imms)) = decode_ubfm(at_u32(&ctx.soa.words, i + 1)) else {
                continue;
            };
            let expected_imms = if lsr_wide { 63u32 } else { 31u32 };
            if lsr_imms != expected_imms || rd_lsr >= 31 || rn_lsr != rd_mov {
                continue;
            }
            let shifted = if sh >= 16 { 0 } else { imm16 >> sh };
            let folded = enc::movz(u32::from(rd_lsr), shifted, 0);
            if rd_lsr == rd_mov && !ctx.soa.is_pinned(i) {
                ctx.soa.replace_word(i, folded);
                ctx.mark_deleted(i + 1);
                stats.movs_eliminated += 1;
            } else {
                ctx.soa.replace_word(i + 1, folded);
            }
        }
    }
}

fn mach_branch_opt<'a>(ctx: &mut MachContext<'a>, stats: &mut MachStats) {
    if !ctx.allow_delete {
        return;
    }

    // 0. Tiny trampoline tail-duplication (+ post-dup `movz #imm16; lsr #sh` folding).
    mach_tail_dup_trampolines(ctx, stats);

    let n = ctx.soa.len();

    // Map word index -> patch index for fast lookup of patched fallthrough `b #0`.
    let has_patch_targets = ctx.patch_target_blocks.len() == ctx.patch_word_idxs.len();
    let mut word_to_patch = vec_filled_in(n, usize::MAX, ctx.alloc);
    for (p, &widx) in ctx.patch_word_idxs.iter().enumerate() {
        if widx != ELIDED_PATCH && widx < n {
            set_at(&mut word_to_patch, widx, p);
        }
    }

    // 1. Delete unconditional `b` at the end of emitted block `pos` targeting `block_order[pos + 1]`
    //    with zero intervening words.
    let num_ord = ctx.block_order.len();
    for pos in 0..num_ord {
        let b = at_usize(&ctx.block_order, pos);
        let b_start = at_usize(&ctx.block_word_off, b);
        let next_start = if pos + 1 < num_ord {
            at_usize(&ctx.block_word_off, at_usize(&ctx.block_order, pos + 1))
        } else {
            n
        };
        if next_start <= b_start || next_start > n {
            continue;
        }
        let last = next_start - 1;
        if ctx.soa.is_deleted(last) || ctx.soa.is_reloc_or_trap(last) || ctx.soa.is_opaque_data(last) {
            continue;
        }
        let DecodedKind::BranchUncond { delta_words } = ctx.soa.kind_at(last) else {
            continue;
        };
        let pidx = at_usize(&word_to_patch, last);
        if pidx != usize::MAX {
            if has_patch_targets && pos + 1 < num_ord {
                let target_b = to_idx(at_u32(&ctx.patch_target_blocks, pidx));
                if target_b == at_usize(&ctx.block_order, pos + 1) {
                    ctx.mark_deleted(last);
                    stats.branches_elided += 1;
                }
            }
        } else if !ctx.soa.is_pinned(last) && delta_words == 1 {
            ctx.mark_deleted(last);
            stats.branches_elided += 1;
        }
    }

    // 2. Also delete any remaining non-patched, non-reloc `b #1` (unconditional jump to very next word).
    for i in 0..n {
        if ctx.soa.is_deleted(i) || ctx.soa.is_pinned(i) || ctx.soa.is_opaque_data(i) {
            continue;
        }
        if let DecodedKind::BranchUncond { delta_words: 1 } = ctx.soa.kind_at(i) {
            ctx.mark_deleted(i);
            stats.branches_elided += 1;
        }
    }
}

// =============================================================================
// 10. Comprehensive Unit Tests (+ Apple Silicon Hardware `exec_leaf` Verification)
// =============================================================================

#[cfg(test)]
mod tests {
    use blaze_own::{Alloc, Vec};

    use super::{
        DecodedKind, ELIDED_PATCH, InstInfo, M5Pipe, MachOptimizeInput, MemEffect, MemSize,
        RET_CALLEE_SAVED_GPRS, RET_LIVE_GPRS, decode_inst, optimize_mach_a64,
    };
    use crate::enc::{
        self, Cond, Lse, SP, Size, X0, X1, X2, X8, X9, X10, X11, X12, X13, X14, X15, X16,
    };

    const X3: u32 = 3;
    const X4: u32 = 4;
    const X5: u32 = 5;
    const X6: u32 = 6;
    const X7: u32 = 7;
    const X17: u32 = 17;
    const X19: u32 = 19;
    const X20: u32 = 20;
    const X21: u32 = 21;
    const X22: u32 = 22;
    const X27: u32 = 27;

    fn test_alloc() -> Alloc<'static> {
        blaze_own::testing::heap().alloc()
    }

    fn words_vec(slice: &[u32], alloc: Alloc<'static>) -> Vec<u32, Alloc<'static>> {
        let mut v = Vec::with_capacity_in(slice.len(), alloc);
        v.extend_from_slice(slice);
        v
    }

    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    fn exec_leaf(words: &[u32], alloc: Alloc<'static>) -> blaze_inproc::exec::Executable {
        let mut bytes: Vec<u8, Alloc<'static>> = Vec::with_capacity_in(words.len() * 4, alloc);
        for &w in words {
            bytes.extend_from_slice(&w.to_le_bytes());
        }
        blaze_inproc::exec::Executable::new(&bytes, 64).expect("W^X exec mapping must succeed")
    }

    /// Resolves a list of `(patch_word_idx, kind_fn_target)` patches in place using `block_word_off`
    /// so we can execute both pre-opt and post-opt machine code bit-for-bit on hardware.
    fn patch_branches(words: &mut [u32], patch_idxs: &[usize], patch_targets: &[u32], block_off: &[usize]) {
        for (p, &widx) in patch_idxs.iter().enumerate() {
            if widx == ELIDED_PATCH {
                continue;
            }
            let tb = usize::try_from(patch_targets[p]).expect("valid target block");
            let dst = block_off[tb];
            let delta = i64::try_from(dst).expect("dst") - i64::try_from(widx).expect("src");
            let w = words[widx];
            let field = if (w & 0xfc00_0000) == 0x1400_0000 {
                enc::Field::Imm26
            } else if (w & 0xff00_0010) == 0x5400_0000 || (w & 0x7e00_0000) == 0x3400_0000 {
                enc::Field::Imm19
            } else {
                enc::Field::Imm14
            };
            words[widx] = field.patch(w, delta * 4).expect("branch target in range");
        }
    }

    #[test]
    fn decode_roundtrip_and_conservative_barrier() {
        // 1. 64-bit `mov x1, x2` vs 32-bit zero-extending `mov w1, w2`.
        let mov64 = decode_inst(enc::mov(X1, X2));
        assert_eq!(mov64.kind, DecodedKind::MovReg { rd: 1, rn: 2, wide: true });
        assert_eq!(mov64.gpr_defs, 1 << 1);
        assert_eq!(mov64.gpr_uses, 1 << 2);
        assert!(!mov64.reads_flags && !mov64.writes_flags && !mov64.is_branch_or_barrier);
        assert_eq!(mov64.pipe, M5Pipe::Alu);

        let mov32 = decode_inst(enc::sf(false, enc::mov(X1, X2)));
        assert_eq!(mov32.kind, DecodedKind::MovReg { rd: 1, rn: 2, wide: false });
        assert_eq!(mov32.gpr_defs, 1 << 1);
        assert_eq!(mov32.gpr_uses, 1 << 2);

        // 2. `add`/`sub` imm & reg, SP handling, and `cmp rn, #0`.
        let add_i = decode_inst(enc::add_imm(X1, X2, 7, false));
        assert_eq!(
            add_i.kind,
            DecodedKind::AddSubImm { rd: 1, rn: 2, imm12: 7, shift12: false, is_sub: false, wide: true, sets_flags: false }
        );
        assert_eq!(add_i.gpr_defs, 1 << 1);
        assert_eq!(add_i.gpr_uses, 1 << 2);

        let sub_sp = decode_inst(enc::sub_imm(SP, SP, 16, false));
        assert_eq!(sub_sp.gpr_defs, 1 << 31);
        assert_eq!(sub_sp.gpr_uses, 1 << 31);
        assert_eq!(sub_sp.mem, MemEffect::Barrier);

        let cmp_z = decode_inst(enc::cmp_imm(X8, 0));
        assert_eq!(cmp_z.kind, DecodedKind::CmpZero { rn: 8, wide: true });
        assert_eq!(cmp_z.gpr_defs, 0); // XZR never sets bit 31!
        assert_eq!(cmp_z.gpr_uses, 1 << 8);
        assert!(cmp_z.writes_flags && !cmp_z.reads_flags);

        let sub_r = decode_inst(enc::sub(X3, X4, X5));
        assert_eq!(
            sub_r.kind,
            DecodedKind::AddSubReg { rd: 3, rn: 4, rm: 5, is_sub: true, wide: true, sets_flags: false }
        );

        // 3. `ldr`/`str` (GPR & FP), `ldur`/`stur`, and latencies.
        let ldr_x = decode_inst(enc::ldr(Size::X, X2, SP, 2));
        assert_eq!(
            ldr_x.kind,
            DecodedKind::LdrOff { rt: 2, rn: 31, imm_bytes: 16, size: MemSize::X64, is_fp: false }
        );
        assert_eq!(ldr_x.gpr_defs, 1 << 2);
        assert_eq!(ldr_x.gpr_uses, 1 << 31);
        assert_eq!(ldr_x.mem, MemEffect::Load);
        assert_eq!(ldr_x.latency, 4);
        assert_eq!(ldr_x.pipe, M5Pipe::LdSt);

        let str_w = decode_inst(enc::str(Size::W, X5, X13, 1));
        assert_eq!(
            str_w.kind,
            DecodedKind::StrOff { rt: 5, rn: 13, imm_bytes: 4, size: MemSize::W32, is_fp: false }
        );
        assert_eq!(str_w.gpr_defs, 0);
        assert_eq!(str_w.gpr_uses, (1 << 5) | (1 << 13));
        assert_eq!(str_w.mem, MemEffect::Store);

        let ldr_q = decode_inst(enc::ldr_q(3, X0, 2));
        assert_eq!(
            ldr_q.kind,
            DecodedKind::LdrOff { rt: 3, rn: 0, imm_bytes: 32, size: MemSize::Q128, is_fp: true }
        );
        assert_eq!(ldr_q.fp_defs, 1 << 3);
        assert_eq!(ldr_q.gpr_defs, 0);

        let ldur_neg = decode_inst(enc::ldur(Size::X, X1, X2, -16));
        assert_eq!(
            ldur_neg.kind,
            DecodedKind::LdrOff { rt: 1, rn: 2, imm_bytes: -16, size: MemSize::X64, is_fp: false }
        );

        // 3b. Post-index loads/stores MUST mark `rn` in BOTH `gpr_uses` and `gpr_defs`!
        let ldr_q_p = decode_inst(enc::ldr_q_post(30, X11, 16));
        assert_eq!(ldr_q_p.gpr_defs, 1 << 11);
        assert_eq!(ldr_q_p.gpr_uses, 1 << 11);
        assert_eq!(ldr_q_p.fp_defs, 1 << 30);
        assert_eq!(ldr_q_p.fp_uses, 0);
        assert_eq!(ldr_q_p.mem, MemEffect::Load);
        assert!(!ldr_q_p.is_branch_or_barrier);

        let str_q_p = decode_inst(enc::str_q_post(30, X10, 16));
        assert_eq!(str_q_p.gpr_defs, 1 << 10);
        assert_eq!(str_q_p.gpr_uses, 1 << 10);
        assert_eq!(str_q_p.fp_defs, 0);
        assert_eq!(str_q_p.fp_uses, 1 << 30);
        assert_eq!(str_q_p.mem, MemEffect::Store);
        assert!(!str_q_p.is_branch_or_barrier);

        let ldr_x_p = decode_inst(enc::ldr_post(X2, X1, 8));
        assert_eq!(ldr_x_p.gpr_defs, (1 << 2) | (1 << 1));
        assert_eq!(ldr_x_p.gpr_uses, 1 << 1);
        assert_eq!(ldr_x_p.mem, MemEffect::Load);
        assert!(!ldr_x_p.is_branch_or_barrier);

        let str_x_p = decode_inst(enc::str_post(X2, X1, 8));
        assert_eq!(str_x_p.gpr_defs, 1 << 1);
        assert_eq!(str_x_p.gpr_uses, (1 << 2) | (1 << 1));
        assert_eq!(str_x_p.mem, MemEffect::Store);
        assert!(!str_x_p.is_branch_or_barrier);

        let ldp_q_p = decode_inst(enc::ldp_q_post(29, 30, X11, 2));
        assert_eq!(ldp_q_p.gpr_defs, 1 << 11);
        assert_eq!(ldp_q_p.gpr_uses, 1 << 11);
        assert_eq!(ldp_q_p.fp_defs, (1 << 29) | (1 << 30));
        assert_eq!(ldp_q_p.mem, MemEffect::Load);
        assert!(!ldp_q_p.is_branch_or_barrier);

        let stp_q_p = decode_inst(enc::stp_q_post(29, 30, X10, 2));
        assert_eq!(stp_q_p.gpr_defs, 1 << 10);
        assert_eq!(stp_q_p.gpr_uses, 1 << 10);
        assert_eq!(stp_q_p.fp_uses, (1 << 29) | (1 << 30));
        assert_eq!(stp_q_p.mem, MemEffect::Store);
        assert!(!stp_q_p.is_branch_or_barrier);

        // 4. `csel`, `cset`, `mul`, `udiv`, `movk` (reads Rd!).
        let csel_i = decode_inst(enc::csel(X22, X8, X3, Cond::Lo));
        assert_eq!(
            csel_i.kind,
            DecodedKind::Csel { rd: 22, rn: 8, rm: 3, cond: 3, wide: true }
        );
        assert!(csel_i.reads_flags && !csel_i.writes_flags);
        assert_eq!(csel_i.gpr_defs, 1 << 22);
        assert_eq!(csel_i.gpr_uses, (1 << 8) | (1 << 3));

        let cset64 = decode_inst(enc::cset(X10, Cond::Lo));
        assert_eq!(cset64.kind, DecodedKind::Cset { rd: 10, cond: 3, wide: true });
        assert_eq!(cset64.gpr_defs, 1 << 10);
        assert_eq!(cset64.gpr_uses, 0);
        assert!(cset64.reads_flags && !cset64.writes_flags);
        assert_eq!(cset64.mem, MemEffect::None);
        assert_eq!(cset64.latency, 1);
        assert_eq!(cset64.pipe, M5Pipe::Alu);

        let cset32 = decode_inst(enc::sf(false, enc::cset(X11, Cond::Eq)));
        assert_eq!(cset32.kind, DecodedKind::Cset { rd: 11, cond: 0, wide: false });
        assert_eq!(cset32.gpr_defs, 1 << 11);
        assert_eq!(cset32.gpr_uses, 0);
        assert!(cset32.reads_flags && !cset32.writes_flags);

        assert_eq!(decode_inst(enc::mul(X0, X1, X2)).latency, 3);
        assert_eq!(decode_inst(enc::udiv(X0, X1, X2)).latency, 8);
        let movk_i = decode_inst(enc::movk(X4, 0x1234, 1));
        assert_eq!(movk_i.gpr_defs, 1 << 4);
        assert_eq!(movk_i.gpr_uses, 1 << 4); // MOVK reads Rd!

        // 5. Branches & calls.
        let b_i = decode_inst(enc::b(-3));
        assert_eq!(b_i.kind, DecodedKind::BranchUncond { delta_words: -3 });
        assert!(b_i.is_branch_or_barrier && b_i.pipe == M5Pipe::Branch);

        let bne_i = decode_inst(enc::b_cond(Cond::Ne, 5));
        assert_eq!(
            bne_i.kind,
            DecodedKind::BranchCond { delta_words: 5, is_flag_cond: true, cond: 1 }
        );
        assert!(bne_i.reads_flags && bne_i.is_branch_or_barrier);

        let bl_i = decode_inst(enc::bl(10));
        assert_eq!(bl_i.kind, DecodedKind::Call);
        assert_eq!(bl_i.mem, MemEffect::Barrier);
        assert!(bl_i.is_branch_or_barrier);

        let ret_i = decode_inst(enc::ret());
        assert_eq!(ret_i.kind, DecodedKind::Ret);
        assert!(ret_i.is_branch_or_barrier);

        // 6. Unknown / exotic / atomic opcodes MUST return full conservative barriers!
        let barrier = InstInfo::conservative_barrier();
        assert_eq!(decode_inst(0x0000_0000), barrier); // UDF #0
        assert_eq!(decode_inst(enc::lse(Lse::Add, Size::X, true, true, X1, X2, X0)), barrier);
        assert_eq!(decode_inst(enc::brk(0)), barrier);
        assert_eq!(decode_inst(enc::mrs_tpidrro_el0(X0)), barrier);
        assert_eq!(decode_inst(0xdead_beef), barrier);
    }

    #[test]
    fn mach_pair_ldp_stp_pairs_across_intervening_alu() {
        let alloc = test_alloc();

        // 1. Positive case: `str w5, [x13]; add x8, x8, #1; str w19, [x13, #4]` -> `stp w5, w19, [x13]`!
        //    Wrapped in a valid AAPCS64 function `extern "C" fn(ptr: *mut u32, cnt: u64, a: u32, b: u32) -> u64`.
        let orig_slice = [
            enc::stp_pre(X19, X20, SP, -16),
            enc::mov(X13, X0),
            enc::mov(X8, X1),
            enc::mov(X5, X2),
            enc::mov(X19, X3),
            enc::str(Size::W, X5, X13, 0),
            enc::add_imm(X8, X8, 1, false),
            enc::str(Size::W, X19, X13, 1),
            enc::mov(X0, X8),
            enc::ldp_post(X19, X20, SP, 16),
            enc::ret(),
        ];
        let mut words = words_vec(&orig_slice, alloc);
        let mut block_off = [0usize];
        let block_order = [0usize];
        let mut patches: [usize; 0] = [];
        let mut relocs: [u32; 0] = [];
        let mut traps: [u32; 0] = [];

        let out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut words,
            block_word_off: &mut block_off,
            block_order: &block_order,
            patch_word_idxs: &mut patches,
            patch_target_blocks: &[],
            reloc_byte_offs: &mut relocs,
            trap_byte_offs: &mut traps,
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert_eq!(out.stats.pairs_fused, 1, "expected str+add+str to fuse into 1 stp");
        let has_stp_w = words.iter().any(|&w| (w & 0xffc0_0000) == 0x2900_0000);
        assert!(has_stp_w, "optimized words must contain stp w*, w*, [rn, #0]");

        // 1b. Exact register-level check: `str w5, [x13]; add x8, x8, #1; str w19, [x13, #4]` -> `stp w5, w19, [x13]`.
        let mut raw_pair = words_vec(
            &[
                enc::str(Size::W, X5, X13, 0),
                enc::add_imm(X8, X8, 1, false),
                enc::str(Size::W, X19, X13, 1),
                enc::ret(),
            ],
            alloc,
        );
        let mut raw_off = [0usize];
        let raw_out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut raw_pair,
            block_word_off: &mut raw_off,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert_eq!(raw_out.stats.pairs_fused, 1);
        assert!(
            raw_pair.iter().any(|&w| w == enc::stp_w_off(X5, X19, X13, 0)),
            "expected exact `stp w5, w19, [x13, #0]` encoding"
        );

        // 2. Negative control: intervening aliasing store MUST prevent pairing!
        let neg_slice = [
            enc::str(Size::W, X5, X13, 0),
            enc::str(Size::W, X9, X10, 0), // may alias [x13, #0..#8]!
            enc::str(Size::W, X19, X13, 1),
            enc::ret(),
        ];
        let mut neg_words = words_vec(&neg_slice, alloc);
        let mut neg_off = [0usize];
        let neg_out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut neg_words,
            block_word_off: &mut neg_off,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert_eq!(neg_out.stats.pairs_fused, 0, "aliasing store between stores must block stp pairing");

        // 3. Hardware execution check on Apple Silicon!
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let ex_orig = exec_leaf(&orig_slice, alloc);
            let ex_opt = exec_leaf(&words, alloc);
            // SAFETY: Both buffers are valid AAPCS64 leaf functions matching this exact signature.
            let f_orig: extern "C" fn(*mut u32, u64, u32, u32) -> u64 = unsafe { ex_orig.as_fn() };
            // SAFETY: Same ABI-verified signature for the optimized machine code buffer.
            let f_opt: extern "C" fn(*mut u32, u64, u32, u32) -> u64 = unsafe { ex_opt.as_fn() };

            for &(cnt, a, b) in &[(0u64, 0x1122_3344u32, 0x5566_7788u32), (41, 0xdead_beef, 0xcafe_babe)] {
                let mut buf_orig = [0u32; 2];
                let mut buf_opt = [0u32; 2];
                let r_orig = f_orig(buf_orig.as_mut_ptr(), cnt, a, b);
                let r_opt = f_opt(buf_opt.as_mut_ptr(), cnt, a, b);
                assert_eq!(r_opt, r_orig, "return value mismatch");
                assert_eq!(r_opt, cnt + 1);
                assert_eq!(buf_opt, buf_orig, "stored pair mismatch");
                assert_eq!(buf_opt, [a, b]);
            }
        }
    }

    #[test]
    fn mach_peephole_flag_fusion_and_csel_latch_mov_elim() {
        let alloc = test_alloc();

        // -----------------------------------------------------------------
        // (a) Flag fusion: `sub x1, x0, #1; cmp x1, #0; b.ne loop` -> `subs x1, x0, #1; b.ne loop`
        // -----------------------------------------------------------------
        let mut flag_orig = words_vec(
            &[
                // block 0 (entry): x1 = x0; x2 = 0
                enc::mov(X1, X0),
                enc::movz(X2, 0, 0),
                // block 1 (loop): x2 += x1; x1 = x1 - 1; cmp x1, #0; b.ne loop
                enc::add(X2, X2, X1),
                enc::sub_imm(X1, X1, 1, false),
                enc::cmp_imm(X1, 0),
                enc::b_cond(Cond::Ne, 0), // patch 0 -> block 1
                // block 2 (exit): x0 = x2; ret
                enc::mov(X0, X2),
                enc::ret(),
            ],
            alloc,
        );
        let mut flag_opt = words_vec(&flag_orig, alloc);
        let mut flag_off = [0usize, 2, 6];
        let flag_order = [0usize, 1, 2];
        let mut flag_patches = [5usize];
        let flag_targets = [1u32];

        // Resolve original for baseline exec.
        patch_branches(&mut flag_orig, &[5], &flag_targets, &[0, 2, 6]);

        let flag_out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut flag_opt,
            block_word_off: &mut flag_off,
            block_order: &flag_order,
            patch_word_idxs: &mut flag_patches,
            patch_target_blocks: &flag_targets,
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        let _ = flag_out;
        assert!(
            flag_opt.iter().any(|&w| w == enc::subs_imm(X1, X1, 1, false)),
            "expected `sub x1, x1, #1; cmp x1, #0` to fuse into `subs x1, x1, #1`"
        );
        assert!(
            !flag_opt.iter().any(|&w| w == enc::cmp_imm(X1, 0)),
            "redundant `cmp x1, #0` must be deleted after fusion"
        );
        patch_branches(&mut flag_opt, &flag_patches, &flag_targets, &flag_off);

        // Negative control: signed `b.gt` reads V flag, so `sub + cmp #0` MUST NOT fuse!
        let mut nofuse_words = words_vec(
            &[
                enc::sub_imm(X1, X0, 1, false),
                enc::cmp_imm(X1, 0),
                enc::b_cond(Cond::Gt, 3),
                enc::movz(X0, 0, 0),
                enc::ret(),
                enc::movz(X0, 1, 0),
                enc::ret(),
            ],
            alloc,
        );
        let mut nofuse_off = [0usize, 5];
        let _ = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut nofuse_words,
            block_word_off: &mut nofuse_off,
            block_order: &[0, 1],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert!(
            nofuse_words.iter().any(|&w| w == enc::cmp_imm(X1, 0)),
            "`cmp #0` feeding `b.gt` (tests V flag) must NOT fuse into `subs`"
        );

        // -----------------------------------------------------------------
        // (b) Exact `decode_exec_leaf_single` pattern: `csel x22, x8, x3, lo; csel x19, x3, x19, lo;
        //     csel x8, x8, x19, eq; cmp ...; csel x8, x0, x8, eq; csel x0, x9, x8, hi` + latch `mov x8, x22`
        // -----------------------------------------------------------------
        // Signature: `extern "C" fn(acc: u64, iters: u64, x3_in: u64, x4_in: u64, x5_in: u64, x6_in: u64, x7_in: u64) -> u64`
        // Entry initializes `x8 = acc ^ 0x11`, `x9 = acc + 3`, `x19 = x3_in + 5`, `x22 = 0`.
        // Loop body mixes `x8` into `x9`, runs the exact `csel` chain, decrements `x1`, branches to `latch` or `exit`.
        let csel_raw = [
            // Block 0 (entry, word 0):
            enc::stp_pre(X19, X22, SP, -16),
            enc::add_imm(X8, X0, 17, false),
            enc::add_imm(X9, X0, 3, false),
            enc::add_imm(X19, X2, 5, false),
            enc::mov(X3, X2),
            // Block 1 (body, word 5):
            enc::add(X9, X9, X8),            // observes loop-carried `x8`!
            enc::add_imm(X4, X4, 1, false),  // varies condition across iterations
            enc::cmp(X4, X5),
            enc::csel(X22, X8, X3, Cond::Lo),
            enc::csel(X19, X3, X19, Cond::Lo),
            enc::csel(X8, X8, X19, Cond::Eq),
            enc::cmp(X6, X7),
            enc::csel(X8, X0, X8, Cond::Eq),
            enc::csel(X0, X9, X8, Cond::Hi),
            enc::subs_imm(X1, X1, 1, false),
            enc::b_cond(Cond::Ne, 0),        // patch 0 -> block 2 (latch)
            enc::b(0),                       // patch 1 -> block 3 (exit)
            // Block 2 (latch, word 17):
            enc::mov(X8, X22),
            enc::b(0),                       // patch 2 -> block 1 (body)
            // Block 3 (exit, word 19):
            enc::ldp_post(X19, X22, SP, 16),
            enc::ret(),
        ];
        let mut csel_orig = words_vec(&csel_raw, alloc);
        let mut csel_opt = words_vec(&csel_raw, alloc);
        let mut csel_off = [0usize, 5, 17, 19];
        let csel_order = [0usize, 1, 2, 3];
        let mut csel_patches = [15usize, 16, 18];
        let csel_targets = [2u32, 3, 1];

        patch_branches(&mut csel_orig, &[15, 16, 18], &csel_targets, &[0, 5, 17, 19]);

        let csel_out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut csel_opt,
            block_word_off: &mut csel_off,
            block_order: &csel_order,
            patch_word_idxs: &mut csel_patches,
            patch_target_blocks: &csel_targets,
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert!(
            csel_out.stats.movs_eliminated >= 1,
            "expected latch `mov x8, x22` to be eliminated, got {:?}",
            csel_out.stats
        );
        assert!(
            !csel_opt.iter().any(|&w| w == enc::mov(X8, X22)),
            "latch `mov x8, x22` must be completely eliminated from optimized code"
        );
        patch_branches(&mut csel_opt, &csel_patches, &csel_targets, &csel_off);

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            // Verify Part (a) on hardware.
            let ex_fo = exec_leaf(&flag_orig, alloc);
            let ex_fp = exec_leaf(&flag_opt, alloc);
            // SAFETY: Valid AAPCS64 leaf function matching `extern "C" fn(u64) -> u64`.
            let f_flag_orig: extern "C" fn(u64) -> u64 = unsafe { ex_fo.as_fn() };
            // SAFETY: Same signature for optimized buffer.
            let f_flag_opt: extern "C" fn(u64) -> u64 = unsafe { ex_fp.as_fn() };
            for &n in &[1u64, 5, 20, 100] {
                assert_eq!(f_flag_opt(n), f_flag_orig(n));
                assert_eq!(f_flag_opt(n), n * (n + 1) / 2);
            }

            // Verify Part (b) on hardware across diverse condition combinations (Lo, Eq, Hi, multi-iter).
            let ex_co = exec_leaf(&csel_orig, alloc);
            let ex_cp = exec_leaf(&csel_opt, alloc);
            // SAFETY: Valid AAPCS64 function saving/restoring X19/X22 matching this 7-arg signature.
            let f_csel_orig: extern "C" fn(u64, u64, u64, u64, u64, u64, u64) -> u64 = unsafe { ex_co.as_fn() };
            // SAFETY: Same signature for optimized buffer.
            let f_csel_opt: extern "C" fn(u64, u64, u64, u64, u64, u64, u64) -> u64 = unsafe { ex_cp.as_fn() };

            let cases: &[(u64, u64, u64, u64, u64, u64, u64)] = &[
                (10, 1, 99, 1, 5, 3, 3),
                (10, 5, 99, 2, 5, 3, 3),
                (42, 8, 777, 0, 4, 10, 2),
                (123, 6, 55, 5, 5, 2, 9),
                (999, 12, 314, 1, 7, 8, 8),
            ];
            for &(acc, iters, x3v, x4v, x5v, x6v, x7v) in cases {
                let want = f_csel_orig(acc, iters, x3v, x4v, x5v, x6v, x7v);
                let got = f_csel_opt(acc, iters, x3v, x4v, x5v, x6v, x7v);
                assert_eq!(
                    got, want,
                    "csel latch mov elimination mismatch for ({acc}, {iters}, {x3v}, {x4v}, {x5v}, {x6v}, {x7v})"
                );
            }
        }
    }

    #[test]
    fn mach_sched_m5_hoists_loads_and_preserves_macro_fusion() {
        let alloc = test_alloc();

        // Function: `extern "C" fn(ptr: *const u64, limit: u64, a: u64, b: u64) -> u64`
        // Straight-line region has independent ALU first, `cmp` in the middle, and a 4-cycle
        // `ldr x6, [x0, #0]` + dependent `add` at the end, followed by `b.ne`.
        // The M5 scheduler MUST:
        //   1. Hoist 4-cycle `ldr x6, [x0, #0]` early ahead of independent ALU!
        //   2. Keep `cmp x2, x1` IMMEDIATELY before `b.ne` for Apple M5 macro-op fusion!
        let ldr_word = enc::ldr(Size::X, X6, X0, 0);
        let cmp_word = enc::cmp(X2, X1);
        let raw = [
            // Block 0 (entry, word 0):
            enc::add_imm(X4, X2, 11, false),
            enc::eor(X5, X3, X4),
            cmp_word,                        // word 2 (separated from `b.ne` before scheduling!)
            enc::add_imm(X7, X5, 3, false),
            ldr_word,                        // word 4 (4-cycle load buried late!)
            enc::add(X8, X6, X7),            // depends on `ldr` + `x7`
            enc::b_cond(Cond::Ne, 0),        // word 6: patch 0 -> block 2
            // Block 1 (eq arm, word 7):
            enc::add(X0, X8, X4),
            enc::ret(),
            // Block 2 (ne arm, word 9):
            enc::eor(X0, X8, X5),
            enc::ret(),
        ];
        let mut orig = words_vec(&raw, alloc);
        let mut opt = words_vec(&raw, alloc);
        let mut block_off = [0usize, 7, 9];
        let block_order = [0usize, 1, 2];
        let mut patches = [6usize];
        let targets = [2u32];

        patch_branches(&mut orig, &[6], &targets, &[0, 7, 9]);

        let out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut opt,
            block_word_off: &mut block_off,
            block_order: &block_order,
            patch_word_idxs: &mut patches,
            patch_target_blocks: &targets,
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert!(out.stats.insts_scheduled > 0, "expected M5 scheduler to reorder instructions");

        let new_ldr_pos = opt.iter().position(|&w| w == ldr_word).expect("ldr must exist");
        let new_cmp_pos = opt.iter().position(|&w| w == cmp_word).expect("cmp must exist");
        let branch_pos = patches[0];

        assert!(
            new_ldr_pos < 4,
            "expected 4-cycle `ldr` to hoist ahead of independent ALU (was 4, now {new_ldr_pos})"
        );
        assert_eq!(
            new_cmp_pos + 1,
            branch_pos,
            "Apple M5 macro-op fusion lock must place `cmp` immediately before `b.ne` (cmp={new_cmp_pos}, b.ne={branch_pos})"
        );

        patch_branches(&mut opt, &patches, &targets, &block_off);

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let ex_orig = exec_leaf(&orig, alloc);
            let ex_opt = exec_leaf(&opt, alloc);
            // SAFETY: Valid AAPCS64 leaf function matching `extern "C" fn(*const u64, u64, u64, u64) -> u64`.
            let f_orig: extern "C" fn(*const u64, u64, u64, u64) -> u64 = unsafe { ex_orig.as_fn() };
            // SAFETY: Same signature for scheduled buffer.
            let f_opt: extern "C" fn(*const u64, u64, u64, u64) -> u64 = unsafe { ex_opt.as_fn() };

            for &(mem_v, lim, a, b) in &[
                (0x1234_5678u64, 5u64, 5u64, 9u64),
                (0xdead_beef, 10, 5, 9),
                (u64::MAX, 0, 0, 42),
                (7, 100, 101, 333),
            ] {
                assert_eq!(f_opt(&mem_v, lim, a, b), f_orig(&mem_v, lim, a, b));
            }
        }
    }

    #[test]
    fn mach_realign_loop_headers_and_remap_extra_words() {
        let alloc = test_alloc();

        // 1. Positive case:
        //    - Block 0 has a dead `mov x9, x1` (overwritten immediately by `movz x9, #42`),
        //      a placeholder `adr x17, #0` in `extra_word_idxs` (must NOT block deletion!),
        //      and a pre-resolved internal branch `cbz x0, +5` targeting Block 2 (word 8).
        //    - Block 1 is an `align16` loop header (originally word 4; deleting word 0 shifts it
        //      to word 3, then `realign_loop_headers` inserts 1 NOP at word 3 so it lands at word 4)
        //      with a live patch `b.ne` and an elided fallthrough `b` to Block 2.
        //    - Block 2 starts with a placeholder SME2 `bl #0` in `extra_word_idxs`.
        let mut words = words_vec(
            &[
                // Block 0 (word 0):
                enc::mov(X9, X1),                 // [0] dead mov -> deleted in Pass 1
                enc::movz(X9, 42, 0),             // [1] overwrites X9 -> moves to [0]
                enc::adr(X17, 0),                 // [2] extra_word_idxs[0] -> moves to [1]
                enc::cbz(X0, 5),                  // [3] internal branch to word 8 (Block 2) -> moves to [2], target [7]
                // Block 1 (word 4, align16):
                enc::add(X2, X2, X0),             // [4] -> [3] after Pass 1 -> [4] after NOP at [3]
                enc::subs_imm(X0, X0, 1, false),  // [5] -> [5]
                enc::b_cond(Cond::Ne, 0),         // [6] patch 0 -> Block 1 -> moves to [6]
                enc::b(0),                        // [7] patch 1 -> Block 2 (fallthrough elided!)
                // Block 2 (word 8):
                enc::bl(0),                       // [8] extra_word_idxs[1] -> moves to [7]
                enc::mov(X0, X2),                 // [9] -> [8]
                enc::ret(),                       // [10] -> [9]
            ],
            alloc,
        );
        let mut block_off = [0usize, 4, 8];
        let block_order = [0usize, 1, 2];
        let mut patches = [6usize, 7];
        let patch_targets = [1u32, 2];
        let mut extra_idxs = [2usize, 8];
        let align16_blocks = [false, true, false];

        let out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut words,
            block_word_off: &mut block_off,
            block_order: &block_order,
            patch_word_idxs: &mut patches,
            patch_target_blocks: &patch_targets,
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut extra_idxs,
            align16_blocks: &align16_blocks,
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });

        assert_eq!(out.stats.movs_eliminated, 1, "placeholder `adr` in extra_word_idxs must not block deletion");
        assert_eq!(out.stats.branches_elided, 1, "fallthrough `b` to Block 2 must be elided");
        assert_eq!(block_off[1] % 4, 0, "loop header Block 1 must be 16-byte aligned");
        assert_eq!(block_off, [0, 4, 7]);
        assert_eq!(words.len(), 10);
        assert_eq!(words[3], enc::nop(), "expected single NOP padding before aligned loop header");
        assert_eq!(words[4], enc::add(X2, X2, X0));

        // Verify remapped `patch_word_idxs`.
        assert_eq!(patches, [6, ELIDED_PATCH]);
        assert_eq!(words[patches[0]], enc::b_cond(Cond::Ne, 0));

        // Verify remapped `extra_word_idxs`.
        assert_eq!(extra_idxs, [1, 7]);
        assert_eq!(words[extra_idxs[0]], enc::adr(X17, 0));
        assert_eq!(words[extra_idxs[1]], enc::bl(0));

        // Verify pre-resolved internal branch `cbz` at word 2 still lands on Block 2 (`2 + 5 == 7`).
        assert_eq!(words[2], enc::cbz(X0, 5));
        assert_eq!(2 + 5, block_off[2]);

        // 2. Negative control: an inline `adr x17, #16` NOT in `extra_word_idxs` MUST set
        //    `allow_delete = false` and skip both deletion and NOP alignment.
        let mut inline_words = words_vec(
            &[
                enc::mov(X9, X1),
                enc::movz(X9, 42, 0),
                enc::adr(X17, 16),
                enc::ret(),
            ],
            alloc,
        );
        let mut inline_off = [0usize, 3];
        let inline_out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut inline_words,
            block_word_off: &mut inline_off,
            block_order: &[0, 1],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[false, true],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert_eq!(inline_out.stats.movs_eliminated, 0);
        assert_eq!(inline_words.len(), 4);
        assert_eq!(inline_off, [0, 3]);
    }

    #[test]
    fn mach_peephole_cset_cmp_csel_eliminates_decode_exec_chain() {
        let alloc = test_alloc();

        // -----------------------------------------------------------------
        // (a) Case 1 (direct, no intervening flag writer): 32-bit `cset` + 64-bit `cmp #0` + 2 `csel`s.
        // Signature: `extern "C" fn(a: u64, b: u64, x_t: u64, x_f: u64) -> u64`
        // -----------------------------------------------------------------
        let direct_raw = [
            enc::cmp(X0, X1),
            enc::sf(false, enc::cset(X10, Cond::Lo)),
            enc::cmp_imm(X10, 0),
            enc::csel(X4, X2, X3, Cond::Ne),
            enc::csel(X5, X2, X3, Cond::Eq),
            enc::add_lsl(X0, X4, X5, 3),
            enc::ret(),
        ];
        let mut direct_opt = words_vec(&direct_raw, alloc);
        let mut direct_off = [0usize];
        let direct_out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut direct_opt,
            block_word_off: &mut direct_off,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert!(
            direct_out.stats.movs_eliminated >= 2,
            "expected Case 1 cset+cmp#0 elimination, got {:?}",
            direct_out.stats
        );
        assert!(
            !direct_opt.iter().any(|&w| matches!(decode_inst(w).kind, DecodedKind::Cset { .. })),
            "Case 1 `cset` must be deleted"
        );
        assert!(
            !direct_opt.iter().any(|&w| matches!(decode_inst(w).kind, DecodedKind::CmpZero { .. })),
            "Case 1 `cmp #0` must be deleted"
        );
        assert_eq!(direct_opt.len(), direct_raw.len() - 2);

        // -----------------------------------------------------------------
        // (b) Case 2 + Case 1 (exact `decode_exec_leaf_single` two-`cmp` + two-`cset` +
        //     two-`cmp #0` + four-`csel` sequence!):
        //     - `csel x14, x5, x12, eq` reads OLD `x5` before `csel x5, x5, x1, ne` writes `x5`
        //     - `cmp w2, #1` intervenes between `cset w10, lo` and `cmp x10, #0`
        //     - Hoisting `csel x5, x5, x1, lo` immediately before `cmp w2, #1` keeps it strictly
        //       AFTER `csel x14, x5, x12, eq`, eliminating ALL 4 wasted instructions (`2 cset + 2 cmp #0`)!
        // Signature: `extern "C" fn(x0_in: u64, x1_in: u64, w2_in: u64, x3_in: u64, x4_in: u64, x5_in: u64) -> u64`
        // -----------------------------------------------------------------
        let chain_raw = [
            enc::add_imm(X12, X0, 100, false),
            enc::add_imm(X13, X3, 200, false),
            enc::add_imm(X15, X4, 300, false),
            enc::sf(false, enc::cmp_imm(X2, 2)),
            enc::sf(false, enc::cset(X10, Cond::Lo)),
            enc::csel(X14, X5, X12, Cond::Eq),
            enc::sf(false, enc::cmp_imm(X2, 1)),
            enc::sf(false, enc::cset(X11, Cond::Eq)),
            enc::cmp_imm(X10, 0),
            enc::csel(X5, X5, X1, Cond::Ne),
            enc::cmp_imm(X11, 0),
            enc::csel(X6, X13, X3, Cond::Ne),
            enc::csel(X7, X15, X4, Cond::Eq),
            enc::add_lsl(X0, X14, X5, 8),
            enc::add_lsl(X0, X0, X6, 16),
            enc::add_lsl(X0, X0, X7, 24),
            enc::ret(),
        ];
        let mut chain_opt = words_vec(&chain_raw, alloc);
        let mut chain_off = [0usize];
        let chain_out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut chain_opt,
            block_word_off: &mut chain_off,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert!(
            chain_out.stats.movs_eliminated >= 4,
            "expected all 4 wasted instructions (2 cset + 2 cmp #0) eliminated, got {:?}",
            chain_out.stats
        );
        assert_eq!(chain_opt.len(), chain_raw.len() - 4);
        assert!(
            !chain_opt.iter().any(|&w| matches!(decode_inst(w).kind, DecodedKind::Cset { .. })),
            "all `cset` instructions must be eliminated"
        );
        assert!(
            !chain_opt.iter().any(|&w| matches!(decode_inst(w).kind, DecodedKind::CmpZero { .. })),
            "all `cmp #0` instructions must be eliminated"
        );

        // -----------------------------------------------------------------
        // (c) Negative control: RAW hazard against intervening flag writer MUST block hoisting!
        // -----------------------------------------------------------------
        let hazard_raw = [
            enc::cmp_imm(X2, 2),
            enc::cset(X10, Cond::Lo),
            enc::subs_imm(X1, X1, 1, false), // writes X1, which `csel` below reads!
            enc::cmp_imm(X10, 0),
            enc::csel(X0, X5, X1, Cond::Ne),
            enc::ret(),
        ];
        let mut hazard_opt = words_vec(&hazard_raw, alloc);
        let mut hazard_off = [0usize];
        let _ = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut hazard_opt,
            block_word_off: &mut hazard_off,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert!(
            hazard_opt.iter().any(|&w| matches!(decode_inst(w).kind, DecodedKind::Cset { .. })),
            "RAW hazard with intervening flag writer must preserve `cset`"
        );
        assert!(
            hazard_opt.iter().any(|&w| matches!(decode_inst(w).kind, DecodedKind::CmpZero { .. })),
            "RAW hazard with intervening flag writer must preserve `cmp #0`"
        );

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            // Verify Part (a) on Apple Silicon hardware.
            let ex_do = exec_leaf(&direct_raw, alloc);
            let ex_dp = exec_leaf(&direct_opt, alloc);
            // SAFETY: Valid AAPCS64 leaf function matching `extern "C" fn(u64, u64, u64, u64) -> u64`.
            let f_direct_orig: extern "C" fn(u64, u64, u64, u64) -> u64 = unsafe { ex_do.as_fn() };
            // SAFETY: Same ABI-verified signature for optimized buffer.
            let f_direct_opt: extern "C" fn(u64, u64, u64, u64) -> u64 = unsafe { ex_dp.as_fn() };
            for &(a, b, xt, xf) in &[
                (0u64, 1u64, 11u64, 29u64),
                (1, 1, 11, 29),
                (5, 2, 11, 29),
                (u64::MAX, 0, 77, 13),
            ] {
                assert_eq!(f_direct_opt(a, b, xt, xf), f_direct_orig(a, b, xt, xf));
            }

            // Verify Part (b) on Apple Silicon hardware for all `w2 in 0..=4`!
            let ex_co = exec_leaf(&chain_raw, alloc);
            let ex_cp = exec_leaf(&chain_opt, alloc);
            // SAFETY: Valid AAPCS64 leaf function matching 6-arg `extern "C" fn(...) -> u64`.
            let f_chain_orig: extern "C" fn(u64, u64, u64, u64, u64, u64) -> u64 = unsafe { ex_co.as_fn() };
            // SAFETY: Same ABI-verified signature for optimized buffer.
            let f_chain_opt: extern "C" fn(u64, u64, u64, u64, u64, u64) -> u64 = unsafe { ex_cp.as_fn() };
            for w2 in 0u64..=4 {
                let want = f_chain_orig(7, 19, w2, 31, 43, 59);
                let got = f_chain_opt(7, 19, w2, 31, 43, 59);
                assert_eq!(got, want, "decode_exec cset+cmp#0+csel mismatch for w2={w2}");
            }
        }
    }

    #[test]
    fn mach_peephole_store_load_reload_elim_and_alias_negative_control() {
        let alloc = test_alloc();

        // -----------------------------------------------------------------
        // (a) Same-register reload: `str x16, [sp]; ...; ldr x16, [sp]` -> `ldr` deleted!
        // Signature: `extern "C" fn(a: u64, b: u64) -> u64`
        // -----------------------------------------------------------------
        let same_raw = [
            enc::sub_imm(SP, SP, 16, false),
            enc::add_imm(X16, X0, 5, false),
            enc::str(Size::X, X16, SP, 0),
            enc::add_imm(X1, X1, 7, false),
            enc::ldr(Size::X, X16, SP, 0),
            enc::add(X0, X16, X1),
            enc::add_imm(SP, SP, 16, false),
            enc::ret(),
        ];
        let mut same_opt = words_vec(&same_raw, alloc);
        let mut same_off = [0usize];
        let same_out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut same_opt,
            block_word_off: &mut same_off,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert!(
            same_out.stats.movs_eliminated >= 1,
            "expected redundant same-reg reload `ldr x16, [sp]` to be deleted, got {:?}",
            same_out.stats
        );
        assert!(
            !same_opt.iter().any(|&w| w == enc::ldr(Size::X, X16, SP, 0)),
            "redundant `ldr x16, [sp]` must be deleted"
        );

        // -----------------------------------------------------------------
        // (b) Cross-register reload: `str x16, [sp]; ldr x5, [sp]` -> replaced by `mov x5, x16`!
        // Signature: `extern "C" fn(a: u64, b: u64) -> u64`
        // -----------------------------------------------------------------
        let cross_raw = [
            enc::sub_imm(SP, SP, 16, false),
            enc::add_imm(X16, X0, 13, false),
            enc::str(Size::X, X16, SP, 0),
            enc::ldr(Size::X, X5, SP, 0),
            enc::add(X16, X16, X1), // modifies X16 so copy-prop retains `mov x5, x16`
            enc::add_lsl(X0, X5, X16, 4),
            enc::add_imm(SP, SP, 16, false),
            enc::ret(),
        ];
        let mut cross_opt = words_vec(&cross_raw, alloc);
        let mut cross_off = [0usize];
        let cross_out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut cross_opt,
            block_word_off: &mut cross_off,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert!(
            cross_out.stats.movs_eliminated >= 1,
            "expected cross-reg reload `ldr x5, [sp]` to be replaced, got {:?}",
            cross_out.stats
        );
        assert!(
            !cross_opt.iter().any(|&w| w == enc::ldr(Size::X, X5, SP, 0)),
            "redundant `ldr x5, [sp]` must be replaced"
        );
        assert!(
            cross_opt.iter().any(|&w| w == enc::mov(X5, X16)),
            "expected `ldr x5, [sp]` to be replaced by `mov x5, x16`"
        );

        // -----------------------------------------------------------------
        // (c) Negative control: intervening aliasing store `str x1, [x2]` MUST preserve `ldr x16, [sp]`!
        // -----------------------------------------------------------------
        let neg_raw = [
            enc::sub_imm(SP, SP, 16, false),
            enc::add_imm(X16, X0, 5, false),
            enc::str(Size::X, X16, SP, 0),
            enc::str(Size::X, X1, X2, 0), // may alias `[sp, #0]`!
            enc::ldr(Size::X, X16, SP, 0),
            enc::add(X0, X16, X1),
            enc::add_imm(SP, SP, 16, false),
            enc::ret(),
        ];
        let mut neg_opt = words_vec(&neg_raw, alloc);
        let mut neg_off = [0usize];
        let neg_out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut neg_opt,
            block_word_off: &mut neg_off,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert_eq!(neg_out.stats.movs_eliminated, 0);
        assert!(
            neg_opt.iter().any(|&w| w == enc::ldr(Size::X, X16, SP, 0)),
            "intervening store `str x1, [x2]` must preserve `ldr x16, [sp]`"
        );

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let ex_so = exec_leaf(&same_raw, alloc);
            let ex_sp = exec_leaf(&same_opt, alloc);
            // SAFETY: Valid AAPCS64 leaf function matching `extern "C" fn(u64, u64) -> u64`.
            let f_same_orig: extern "C" fn(u64, u64) -> u64 = unsafe { ex_so.as_fn() };
            // SAFETY: Same ABI-verified signature for optimized buffer.
            let f_same_opt: extern "C" fn(u64, u64) -> u64 = unsafe { ex_sp.as_fn() };

            let ex_co = exec_leaf(&cross_raw, alloc);
            let ex_cp = exec_leaf(&cross_opt, alloc);
            // SAFETY: Valid AAPCS64 leaf function matching `extern "C" fn(u64, u64) -> u64`.
            let f_cross_orig: extern "C" fn(u64, u64) -> u64 = unsafe { ex_co.as_fn() };
            // SAFETY: Same ABI-verified signature for optimized buffer.
            let f_cross_opt: extern "C" fn(u64, u64) -> u64 = unsafe { ex_cp.as_fn() };

            for &(a, b) in &[(0u64, 0u64), (17, 23), (0x1234, 0x5678), (999_999, 42)] {
                assert_eq!(f_same_opt(a, b), f_same_orig(a, b));
                assert_eq!(f_cross_opt(a, b), f_cross_orig(a, b));
            }
        }
    }

    #[test]
    fn mach_peephole_header_entry_latch_mov_with_entry_negative_control() {
        let alloc = test_alloc();

        // -----------------------------------------------------------------
        // (a) Positive: rotated multi-arm loop latch entered ONLY from `body_inner`, with
        //     `entry` jumping AFTER `mov x5, x21` directly into `body_inner`!
        //     Signature: `extern "C" fn(iters: u64, x1: u64, x2: u64, x3: u64, x5_init: u64, x7: u64) -> u64`
        // -----------------------------------------------------------------
        let pos_raw = [
            // Block 0 (entry, word 0..8):
            enc::stp_pre(X19, X21, SP, -16),                  // [0]
            enc::mov(X7, X5),                                 // [1] x7 = x7_in (arg 5)
            enc::mov(X5, X4),                                 // [2] x5 = x5_init (arg 4)
            enc::mov(X4, X0),                                 // [3] x4 = iters (arg 0)
            enc::add_imm(X19, X2, 5, false),                  // [4] x19 = x2_in + 5
            enc::add_imm(X15, X3, 11, false),                 // [5] x15 = x3_in + 11
            enc::movz(X14, 0, 0),                             // [6] x14 = 0
            enc::b(0),                                        // [7] patch 0 -> Block 3 (AFTER mov x5, x21)
            // Block 1 (loop_latch, word 8..10):
            enc::subs_imm(X4, X4, 1, false),                  // [8]
            enc::b_cond(Cond::Eq, 0),                         // [9] patch 1 -> Block 6 (exit)
            // Block 2 (header_mov / l_mbb, word 10..11):
            enc::mov(X5, X21),                                // [10] latch copy at HEAD of loop body
            // Block 3 (body_inner / def_mbb, word 11..24):
            enc::add(X14, X14, X5),                           // [11] observes loop-carried x5
            enc::add_imm(X7, X7, 1, false),                   // [12] varies conditions across iters
            enc::cmp_imm(X7, 4),                              // [13]
            enc::csel(X21, X5, X1, Cond::Lo),                 // [14] writes x21, reads old x5
            enc::csel(X5, X5, X19, Cond::Eq),                 // [15] reads old x5, overwrites x5 as temp
            enc::and(X10, X7, X2),                            // [16] w10 = x7 & x2
            enc::sf(false, enc::cmp_imm(X10, 0)),             // [17] cmp w10, #0
            enc::csel(X5, X15, X5, Cond::Eq),                 // [18] still temp x5
            enc::csel(X1, X1, X15, Cond::Eq),                 // [19]
            enc::sf(false, enc::subs_imm(X10, X7, 3, false)), // [20] subs w10, w7, #3
            enc::csel(X15, X10, X5, Cond::Hi),                // [21] last use of temp x5
            enc::add(X14, X14, X15),                          // [22] observes x15
            enc::cbz(X10, 0),                                 // [23] patch 2 -> Block 5 (arm1)
            // Block 4 (arm0, word 24..26):
            enc::add_imm(X14, X14, 7, false),                 // [24]
            enc::b(0),                                        // [25] patch 3 -> Block 1 (loop_latch)
            // Block 5 (arm1, word 26..28):
            enc::add_imm(X14, X14, 13, false),                // [26]
            enc::b(0),                                        // [27] patch 4 -> Block 1 (loop_latch)
            // Block 6 (exit, word 28..32):
            enc::mov(X0, X14),                                // [28]
            enc::movz(X5, 0, 0),                              // [29] overwrites x5 before ret
            enc::ldp_post(X19, X21, SP, 16),                  // [30]
            enc::ret(),                                       // [31]
        ];
        let mut pos_orig = words_vec(&pos_raw, alloc);
        let mut pos_opt = words_vec(&pos_raw, alloc);
        let mut pos_off = [0usize, 8, 10, 11, 24, 26, 28];
        let pos_order = [0usize, 1, 2, 3, 4, 5, 6];
        let mut pos_patches = [7usize, 9, 23, 25, 27];
        let pos_targets = [3u32, 6, 5, 1, 1];

        patch_branches(&mut pos_orig, &[7, 9, 23, 25, 27], &pos_targets, &[0, 8, 10, 11, 24, 26, 28]);

        let pos_out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut pos_opt,
            block_word_off: &mut pos_off,
            block_order: &pos_order,
            patch_word_idxs: &mut pos_patches,
            patch_target_blocks: &pos_targets,
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert!(
            pos_out.stats.movs_eliminated >= 1,
            "expected header-entry latch `mov x5, x21` to be eliminated, got {:?}",
            pos_out.stats
        );
        assert!(
            !pos_opt.iter().any(|&w| w == enc::mov(X5, X21)),
            "header-entry latch `mov x5, x21` must be eliminated when entry bypasses it"
        );
        patch_branches(&mut pos_opt, &pos_patches, &pos_targets, &pos_off);

        // -----------------------------------------------------------------
        // (b) Negative control 1: `entry` jumps BEFORE `mov x5, x21` (with `x21` set in entry).
        //     Reaching-defs check MUST detect multiple defs (`mbb 0` + `body_inner`) and preserve `mov x5, x21`!
        // -----------------------------------------------------------------
        let mut neg_raw = pos_raw;
        neg_raw[2] = enc::mov(X21, X4); // entry passes initial value in x21 (needs mov x5, x21 on iter 0!)
        let mut neg_orig = words_vec(&neg_raw, alloc);
        let mut neg_opt = words_vec(&neg_raw, alloc);
        let mut neg_off = [0usize, 8, 10, 11, 24, 26, 28];
        let mut neg_patches = [7usize, 9, 23, 25, 27];
        let neg_targets = [2u32, 6, 5, 1, 1]; // patch 0 -> Block 2 (BEFORE mov x5, x21!)

        patch_branches(&mut neg_orig, &[7, 9, 23, 25, 27], &neg_targets, &[0, 8, 10, 11, 24, 26, 28]);

        let _ = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut neg_opt,
            block_word_off: &mut neg_off,
            block_order: &pos_order,
            patch_word_idxs: &mut neg_patches,
            patch_target_blocks: &neg_targets,
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert!(
            neg_opt.iter().any(|&w| w == enc::mov(X5, X21)),
            "when `entry` jumps BEFORE `mov x5, x21`, reaching-defs must preserve `mov x5, x21`"
        );
        patch_branches(&mut neg_opt, &neg_patches, &neg_targets, &neg_off);

        // -----------------------------------------------------------------
        // (c) Negative control 2: `entry` (mbb 0) falls into `loop_latch` without defining `x_tmp` (`x2`),
        //     so backward BFS reaches function entry `mbb 0` and MUST preserve `mov x5, x2`!
        // -----------------------------------------------------------------
        let entry_pass_raw = [
            // Block 0 (entry, word 0..3): does NOT touch x5 or x2; jumps to Block 2 (mov x5, x2)
            enc::mov(X4, X0),                 // [0]
            enc::movz(X14, 0, 0),             // [1]
            enc::b(0),                        // [2] patch 0 -> Block 2
            // Block 1 (loop_latch, word 3..5):
            enc::subs_imm(X4, X4, 1, false),  // [3]
            enc::b_cond(Cond::Eq, 0),         // [4] patch 1 -> Block 4 (exit)
            // Block 2 (header_mov, word 5..6):
            enc::mov(X5, X2),                 // [5] MUST be preserved (x2 arrives from function entry mbb 0!)
            // Block 3 (body, word 6..12):
            enc::add(X14, X14, X5),           // [6]
            enc::cmp_imm(X4, 2),              // [7]
            enc::csel(X2, X5, X1, Cond::Lo),  // [8]
            enc::csel(X5, X5, X3, Cond::Eq),  // [9]
            enc::add(X14, X14, X5),           // [10]
            enc::b(0),                        // [11] patch 2 -> Block 1
            // Block 4 (exit, word 12..15):
            enc::mov(X0, X14),                // [12]
            enc::movz(X5, 0, 0),              // [13]
            enc::ret(),                       // [14]
        ];
        let mut ep_opt = words_vec(&entry_pass_raw, alloc);
        let mut ep_off = [0usize, 3, 5, 6, 12];
        let mut ep_patches = [2usize, 4, 11];
        let ep_targets = [2u32, 4, 1];
        let _ = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut ep_opt,
            block_word_off: &mut ep_off,
            block_order: &[0, 1, 2, 3, 4],
            patch_word_idxs: &mut ep_patches,
            patch_target_blocks: &ep_targets,
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert!(
            ep_opt.iter().any(|&w| w == enc::mov(X5, X2)),
            "reaching function entry `mbb 0` without passing through def_mbb must preserve `mov x5, x2`"
        );

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let ex_po = exec_leaf(&pos_orig, alloc);
            let ex_pp = exec_leaf(&pos_opt, alloc);
            // SAFETY: Valid AAPCS64 leaf function saving/restoring X19/X21 matching this 6-arg signature.
            let f_pos_orig: extern "C" fn(u64, u64, u64, u64, u64, u64) -> u64 = unsafe { ex_po.as_fn() };
            // SAFETY: Same ABI-verified signature for optimized buffer.
            let f_pos_opt: extern "C" fn(u64, u64, u64, u64, u64, u64) -> u64 = unsafe { ex_pp.as_fn() };

            let ex_no = exec_leaf(&neg_orig, alloc);
            let ex_np = exec_leaf(&neg_opt, alloc);
            // SAFETY: Valid AAPCS64 leaf function saving/restoring X19/X21 matching this 6-arg signature.
            let f_neg_orig: extern "C" fn(u64, u64, u64, u64, u64, u64) -> u64 = unsafe { ex_no.as_fn() };
            // SAFETY: Same ABI-verified signature for optimized negative-control buffer.
            let f_neg_opt: extern "C" fn(u64, u64, u64, u64, u64, u64) -> u64 = unsafe { ex_np.as_fn() };

            let cases: &[(u64, u64, u64, u64, u64, u64)] = &[
                (1, 17, 3, 29, 101, 0),
                (5, 17, 3, 29, 101, 0),
                (8, 42, 1, 77, 999, 2),
                (12, 7, 5, 13, 314, 1),
            ];
            for &(iters, x1v, x2v, x3v, x5v, x7v) in cases {
                assert_eq!(
                    f_pos_opt(iters, x1v, x2v, x3v, x5v, x7v),
                    f_pos_orig(iters, x1v, x2v, x3v, x5v, x7v),
                    "positive header-entry latch mov elimination mismatch"
                );
                assert_eq!(
                    f_neg_opt(iters, x1v, x2v, x3v, x5v, x7v),
                    f_neg_orig(iters, x1v, x2v, x3v, x5v, x7v),
                    "negative-control header-entry latch mov mismatch"
                );
            }
        }
    }

    #[test]
    fn mach_peephole_redundant_cmp_zero_after_subs() {
        let alloc = test_alloc();

        // -----------------------------------------------------------------
        // (a) Positive (`subs` + intervening non-flag ALU + `cmp #0` + `b.eq`):
        //     Signature: `extern "C" fn(n: u64, acc: u64) -> u64`
        // -----------------------------------------------------------------
        let pos_raw = [
            // Block 0 (loop, word 0..5):
            enc::subs_imm(X0, X0, 1, false), // [0] sets N,Z,C,V and writes X0
            enc::add(X1, X1, X0),            // [1] intervening ALU (neither writes flags nor X0)
            enc::cmp_imm(X0, 0),             // [2] REDUNDANT! b.eq only reads Z!
            enc::b_cond(Cond::Eq, 0),        // [3] patch 0 -> Block 1 (exit)
            enc::b(0),                       // [4] patch 1 -> Block 0 (loop)
            // Block 1 (exit, word 5..7):
            enc::mov(X0, X1),                // [5]
            enc::ret(),                      // [6]
        ];
        let mut pos_orig = words_vec(&pos_raw, alloc);
        let mut pos_opt = words_vec(&pos_raw, alloc);
        let mut pos_off = [0usize, 5];
        let mut pos_patches = [3usize, 4];
        let pos_targets = [1u32, 0];

        patch_branches(&mut pos_orig, &[3, 4], &pos_targets, &[0, 5]);

        let pos_out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut pos_opt,
            block_word_off: &mut pos_off,
            block_order: &[0, 1],
            patch_word_idxs: &mut pos_patches,
            patch_target_blocks: &pos_targets,
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert!(
            pos_out.stats.movs_eliminated >= 1,
            "expected redundant `cmp x0, #0` after `subs` to be deleted, got {:?}",
            pos_out.stats
        );
        assert!(
            !pos_opt.iter().any(|&w| w == enc::cmp_imm(X0, 0)),
            "redundant `cmp x0, #0` feeding `b.eq` must be deleted"
        );
        assert!(
            pos_opt.iter().any(|&w| w == enc::subs_imm(X0, X0, 1, false)),
            "original `subs x0, x0, #1` must remain"
        );
        patch_branches(&mut pos_opt, &pos_patches, &pos_targets, &pos_off);

        // -----------------------------------------------------------------
        // (b) Positive (`ands` + `cmp #0` + `b.ne`):
        //     Signature: `extern "C" fn(a: u64, b: u64) -> u64`
        // -----------------------------------------------------------------
        let ands_raw = [
            // Block 0 (word 0..5):
            enc::and(X2, X0, X1) | (3u32 << 29), // [0] `ands x2, x0, x1`
            enc::cmp_imm(X2, 0),                 // [1] REDUNDANT! b.ne only reads Z!
            enc::b_cond(Cond::Ne, 0),            // [2] patch 0 -> Block 1
            enc::movz(X0, 10, 0),                // [3]
            enc::ret(),                          // [4]
            // Block 1 (word 5..7):
            enc::mov(X0, X2),                    // [5]
            enc::ret(),                          // [6]
        ];
        let mut ands_orig = words_vec(&ands_raw, alloc);
        let mut ands_opt = words_vec(&ands_raw, alloc);
        let mut ands_off = [0usize, 5];
        let mut ands_patches = [2usize];
        let ands_targets = [1u32];

        patch_branches(&mut ands_orig, &[2], &ands_targets, &[0, 5]);

        let ands_out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut ands_opt,
            block_word_off: &mut ands_off,
            block_order: &[0, 1],
            patch_word_idxs: &mut ands_patches,
            patch_target_blocks: &ands_targets,
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert!(
            ands_out.stats.movs_eliminated >= 1,
            "expected redundant `cmp x2, #0` after `ands` to be deleted, got {:?}",
            ands_out.stats
        );
        assert!(
            !ands_opt.iter().any(|&w| w == enc::cmp_imm(X2, 0)),
            "redundant `cmp x2, #0` after `ands` must be deleted"
        );
        patch_branches(&mut ands_opt, &ands_patches, &ands_targets, &ands_off);

        // -----------------------------------------------------------------
        // (c) Negative control (`subs` + `cmp #0` + `b.lo`):
        //     `b.lo` reads `C` flag (`C == 0` on borrow from `subs`, vs `C == 1` ALWAYS from `cmp #0`).
        //     MUST preserve `cmp x2, #0`!
        // -----------------------------------------------------------------
        let neg_raw = [
            // Block 0 (word 0..5):
            enc::sub(X2, X0, X1) | (1u32 << 29), // [0] `subs x2, x0, x1`
            enc::cmp_imm(X2, 0),                 // [1] MUST NOT be deleted for `b.lo`!
            enc::b_cond(Cond::Lo, 0),            // [2] patch 0 -> Block 1 (reads C flag!)
            enc::movz(X0, 42, 0),                // [3]
            enc::ret(),                          // [4]
            // Block 1 (word 5..7):
            enc::movz(X0, 99, 0),                // [5]
            enc::ret(),                          // [6]
        ];
        let mut neg_orig = words_vec(&neg_raw, alloc);
        let mut neg_opt = words_vec(&neg_raw, alloc);
        let mut neg_off = [0usize, 5];
        let mut neg_patches = [2usize];
        let neg_targets = [1u32];

        patch_branches(&mut neg_orig, &[2], &neg_targets, &[0, 5]);

        let neg_out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut neg_opt,
            block_word_off: &mut neg_off,
            block_order: &[0, 1],
            patch_word_idxs: &mut neg_patches,
            patch_target_blocks: &neg_targets,
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert_eq!(neg_out.stats.movs_eliminated, 0);
        assert!(
            neg_opt.iter().any(|&w| w == enc::cmp_imm(X2, 0)),
            "`cmp x2, #0` feeding `b.lo` (reads C flag) must NOT be deleted after `subs`"
        );
        patch_branches(&mut neg_opt, &neg_patches, &neg_targets, &neg_off);

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let ex_po = exec_leaf(&pos_orig, alloc);
            let ex_pp = exec_leaf(&pos_opt, alloc);
            // SAFETY: Valid AAPCS64 leaf function matching `extern "C" fn(u64, u64) -> u64`.
            let f_pos_orig: extern "C" fn(u64, u64) -> u64 = unsafe { ex_po.as_fn() };
            // SAFETY: Same ABI-verified signature for optimized buffer.
            let f_pos_opt: extern "C" fn(u64, u64) -> u64 = unsafe { ex_pp.as_fn() };
            for &(n, acc) in &[(1u64, 0u64), (2, 7), (5, 100), (20, 3)] {
                assert_eq!(f_pos_opt(n, acc), f_pos_orig(n, acc));
            }

            let ex_ao = exec_leaf(&ands_orig, alloc);
            let ex_ap = exec_leaf(&ands_opt, alloc);
            // SAFETY: Valid AAPCS64 leaf function matching `extern "C" fn(u64, u64) -> u64`.
            let f_ands_orig: extern "C" fn(u64, u64) -> u64 = unsafe { ex_ao.as_fn() };
            // SAFETY: Same ABI-verified signature for optimized buffer.
            let f_ands_opt: extern "C" fn(u64, u64) -> u64 = unsafe { ex_ap.as_fn() };
            for &(a, b) in &[(0u64, 5u64), (0b1010, 0b0101), (0b1100, 0b0110), (u64::MAX, 1)] {
                assert_eq!(f_ands_opt(a, b), f_ands_orig(a, b));
            }

            let ex_no = exec_leaf(&neg_orig, alloc);
            let ex_np = exec_leaf(&neg_opt, alloc);
            // SAFETY: Valid AAPCS64 leaf function matching `extern "C" fn(u64, u64) -> u64`.
            let f_neg_orig: extern "C" fn(u64, u64) -> u64 = unsafe { ex_no.as_fn() };
            // SAFETY: Same ABI-verified signature for optimized buffer.
            let f_neg_opt: extern "C" fn(u64, u64) -> u64 = unsafe { ex_np.as_fn() };
            for &(a, b) in &[(3u64, 10u64), (10, 10), (20, 5), (0, 1)] {
                assert_eq!(f_neg_opt(a, b), f_neg_orig(a, b));
                assert_eq!(f_neg_opt(a, b), 42);
            }
        }
    }

    #[test]
    fn mach_fuse_post_index_fuses_32b_and_16b_copy_loops_and_executes() {
        let alloc = test_alloc();

        // -----------------------------------------------------------------
        // Test 1: Exact `decode_exec_leaf_single` 32B copy loop:
        //   ldp q29, q30, [x1]; add x1, x1, #32; stp q29, q30, [x0]; add x0, x0, #32; cmp x0, x2; b.lo
        //   -> shrinks from 6 loop instructions to 4 (`ldp ... [x1], #32; stp ... [x0], #32; cmp; b.lo`)!
        // Signature: `extern "C" fn(dst: *mut u8, src: *const u8, dst_end: *const u8)`
        // -----------------------------------------------------------------
        let copy32_raw = [
            // Block 0 (loop, 6 words -> 4 words):
            enc::ldp_q_off(29, 30, X1, 0),
            enc::add_imm(X1, X1, 32, false),
            enc::stp_q_off(29, 30, X0, 0),
            enc::add_imm(X0, X0, 32, false),
            enc::cmp(X0, X2),
            enc::b_cond(Cond::Lo, 0), // patch 0 -> Block 0
            // Block 1 (exit):
            enc::ret(),
        ];
        let mut copy32_orig = words_vec(&copy32_raw, alloc);
        let mut copy32_opt = words_vec(&copy32_raw, alloc);
        let mut copy32_off = [0usize, 6];
        let mut copy32_patches = [5usize];
        let copy32_targets = [0u32];

        patch_branches(&mut copy32_orig, &[5], &copy32_targets, &[0, 6]);

        let copy32_out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut copy32_opt,
            block_word_off: &mut copy32_off,
            block_order: &[0, 1],
            patch_word_idxs: &mut copy32_patches,
            patch_target_blocks: &copy32_targets,
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert_eq!(copy32_out.stats.pairs_fused, 2, "expected ldp+add and stp+add to fuse");
        assert_eq!(copy32_off, [0, 4], "32B copy loop must shrink from 6 words to 4");
        assert_eq!(copy32_opt.len(), 5);
        assert_eq!(copy32_opt[0], enc::ldp_q_post(29, 30, X1, 2));
        assert_eq!(copy32_opt[1], enc::stp_q_post(29, 30, X0, 2));
        assert_eq!(copy32_opt[2], enc::cmp(X0, X2));
        assert_eq!(copy32_patches, [3]);
        assert_eq!(copy32_opt[4], enc::ret());
        patch_branches(&mut copy32_opt, &copy32_patches, &copy32_targets, &copy32_off);

        // -----------------------------------------------------------------
        // Test 2: Exact 16B copy loop:
        //   ldr q30, [x1]; add x1, x1, #16; str q30, [x0]; add x0, x0, #16; cmp x0, x2; b.lo
        //   -> shrinks from 6 loop instructions to 4 (`ldr q30, [x1], #16; str q30, [x0], #16; cmp; b.lo`)!
        // -----------------------------------------------------------------
        let copy16_raw = [
            // Block 0 (loop, 6 words -> 4 words):
            enc::ldr_q(30, X1, 0),
            enc::add_imm(X1, X1, 16, false),
            enc::str_q(30, X0, 0),
            enc::add_imm(X0, X0, 16, false),
            enc::cmp(X0, X2),
            enc::b_cond(Cond::Lo, 0), // patch 0 -> Block 0
            // Block 1 (exit):
            enc::ret(),
        ];
        let mut copy16_orig = words_vec(&copy16_raw, alloc);
        let mut copy16_opt = words_vec(&copy16_raw, alloc);
        let mut copy16_off = [0usize, 6];
        let mut copy16_patches = [5usize];
        let copy16_targets = [0u32];

        patch_branches(&mut copy16_orig, &[5], &copy16_targets, &[0, 6]);

        let copy16_out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut copy16_opt,
            block_word_off: &mut copy16_off,
            block_order: &[0, 1],
            patch_word_idxs: &mut copy16_patches,
            patch_target_blocks: &copy16_targets,
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert_eq!(copy16_out.stats.pairs_fused, 2, "expected ldr_q+add and str_q+add to fuse");
        assert_eq!(copy16_off, [0, 4], "16B copy loop must shrink from 6 words to 4");
        assert_eq!(copy16_opt.len(), 5);
        assert_eq!(copy16_opt[0], enc::ldr_q_post(30, X1, 16));
        assert_eq!(copy16_opt[1], enc::str_q_post(30, X0, 16));
        assert_eq!(copy16_opt[2], enc::cmp(X0, X2));
        assert_eq!(copy16_patches, [3]);
        assert_eq!(copy16_opt[4], enc::ret());
        patch_branches(&mut copy16_opt, &copy16_patches, &copy16_targets, &copy16_off);

        // -----------------------------------------------------------------
        // Test 3: Negative controls — MUST NOT fuse!
        // -----------------------------------------------------------------
        // (3a) Intervening use of `rn` (`ldr w3, [x1, #8]` between `ldr x2, [x1]` and `add x1, x1, #8`).
        let mut neg_use = words_vec(
            &[
                enc::ldr(Size::X, X2, X1, 0),
                enc::ldr(Size::W, X3, X1, 2), // reads x1 at byte offset 8!
                enc::add_imm(X1, X1, 8, false),
                enc::add(X0, X2, X3),
                enc::add(X0, X0, X1),
                enc::ret(),
            ],
            alloc,
        );
        let mut neg_use_off = [0usize];
        let neg_use_out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut neg_use,
            block_word_off: &mut neg_use_off,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert_eq!(neg_use_out.stats.pairs_fused, 0, "intervening read of rn must block post-index fusion");
        assert!(neg_use.iter().any(|&w| w == enc::add_imm(X1, X1, 8, false)));

        // (3b) `rt == rn` on GPR load (`ldr x0, [x0]; add x0, x0, #8`) and store (`str x1, [x1]; add x1, x1, #8`).
        let mut neg_rt_eq_rn = words_vec(
            &[
                enc::ldr(Size::X, X0, X0, 0),
                enc::add_imm(X0, X0, 8, false),
                enc::ret(),
            ],
            alloc,
        );
        let mut neg_rt_off = [0usize];
        let neg_rt_out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut neg_rt_eq_rn,
            block_word_off: &mut neg_rt_off,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert_eq!(neg_rt_out.stats.pairs_fused, 0, "GPR rt == rn must block post-index fusion");
        assert_eq!(neg_rt_eq_rn.len(), 3);

        let mut neg_str_rt_eq_rn = words_vec(
            &[
                enc::str(Size::X, X1, X1, 0),
                enc::add_imm(X1, X1, 8, false),
                enc::add(X0, X1, X2),
                enc::ret(),
            ],
            alloc,
        );
        let mut neg_str_rt_off = [0usize];
        let neg_str_rt_out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut neg_str_rt_eq_rn,
            block_word_off: &mut neg_str_rt_off,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert_eq!(neg_str_rt_out.stats.pairs_fused, 0, "GPR str rt == rn must block post-index fusion");
        assert_eq!(neg_str_rt_eq_rn.len(), 4);

        // (3c) `rn == SP`: never mutate SP via post-index fusion!
        let mut neg_sp = words_vec(
            &[
                enc::sub_imm(SP, SP, 16, false),
                enc::ldr(Size::X, X2, SP, 0),
                enc::add_imm(SP, SP, 16, false),
                enc::add(X0, X2, X1),
                enc::ret(),
            ],
            alloc,
        );
        let mut neg_sp_off = [0usize];
        let neg_sp_out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut neg_sp,
            block_word_off: &mut neg_sp_off,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert_eq!(neg_sp_out.stats.pairs_fused, 0, "SP base register must block post-index fusion");
        assert_eq!(neg_sp.len(), 5);

        // Hardware execution checks on Apple Silicon!
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            // Verify Test 1 (32B copy loop) bit-exact on a 64B buffer.
            let ex32_orig = exec_leaf(&copy32_orig, alloc);
            let ex32_opt = exec_leaf(&copy32_opt, alloc);
            // SAFETY: Valid AAPCS64 leaf function (only touches caller-saved x0..x2 and q29..q30).
            let f32_orig: extern "C" fn(*mut u8, *const u8, *const u8) = unsafe { ex32_orig.as_fn() };
            // SAFETY: Same ABI-verified signature for post-index fused 32B copy loop.
            let f32_opt: extern "C" fn(*mut u8, *const u8, *const u8) = unsafe { ex32_opt.as_fn() };

            let mut src64 = [0u8; 64];
            for (idx, b) in src64.iter_mut().enumerate() {
                *b = u8::try_from((idx * 13 + 7) & 0xff).unwrap_or(0);
            }
            let mut dst64_orig = [0u8; 64];
            let mut dst64_opt = [0u8; 64];
            let p_orig = dst64_orig.as_mut_ptr();
            f32_orig(p_orig, src64.as_ptr(), p_orig.wrapping_add(dst64_orig.len()));
            let p_opt = dst64_opt.as_mut_ptr();
            f32_opt(p_opt, src64.as_ptr(), p_opt.wrapping_add(dst64_opt.len()));
            assert_eq!(dst64_opt, src64, "fused 32B copy loop must match source bytes");
            assert_eq!(dst64_opt, dst64_orig, "fused 32B copy loop must match unfused baseline");

            // Verify Test 2 (16B copy loop) bit-exact on a 48B buffer.
            let ex16_orig = exec_leaf(&copy16_orig, alloc);
            let ex16_opt = exec_leaf(&copy16_opt, alloc);
            // SAFETY: Valid AAPCS64 leaf function (only touches caller-saved x0..x2 and q30).
            let f16_orig: extern "C" fn(*mut u8, *const u8, *const u8) = unsafe { ex16_orig.as_fn() };
            // SAFETY: Same ABI-verified signature for post-index fused 16B copy loop.
            let f16_opt: extern "C" fn(*mut u8, *const u8, *const u8) = unsafe { ex16_opt.as_fn() };

            let mut src48 = [0u8; 48];
            for (idx, b) in src48.iter_mut().enumerate() {
                *b = u8::try_from((idx * 29 + 11) & 0xff).unwrap_or(0);
            }
            let mut dst48_orig = [0u8; 48];
            let mut dst48_opt = [0u8; 48];
            let q_orig = dst48_orig.as_mut_ptr();
            f16_orig(q_orig, src48.as_ptr(), q_orig.wrapping_add(dst48_orig.len()));
            let q_opt = dst48_opt.as_mut_ptr();
            f16_opt(q_opt, src48.as_ptr(), q_opt.wrapping_add(dst48_opt.len()));
            assert_eq!(dst48_opt, src48, "fused 16B copy loop must match source bytes");
            assert_eq!(dst48_opt, dst48_orig, "fused 16B copy loop must match unfused baseline");
        }
    }

    #[test]
    fn peephole_sub1_cinc_cmp0_fuses_to_cinc_subs1_for_eq_csel() {
        let alloc = test_alloc();

        // Exact pattern:
        //   adds w11, w14, w2
        //   sub  w2, w6, #1
        //   add  x7, x7, #1
        //   cinc w2, w2, eq        (csinc w2, w2, w2, ne)
        //   cmp  w2, #0
        //   csel x14, x15, x14, eq
        //   cmp  w2, #2
        //   csel x12, x1, x12, lo
        //   ret
        let raw = [
            enc::sf(false, enc::adds(X11, X14, X2)),
            enc::sf(false, enc::sub_imm(X2, X6, 1, false)),
            enc::add_imm(X7, X7, 1, false),
            enc::sf(false, enc::csinc(X2, X2, X2, Cond::Ne)),
            enc::sf(false, enc::cmp_imm(X2, 0)),
            enc::csel(X14, X15, X14, Cond::Eq),
            enc::sf(false, enc::cmp_imm(X2, 2)),
            enc::csel(X12, X1, X12, Cond::Lo),
            enc::ret(),
        ];
        let mut opt = words_vec(&raw, alloc);
        let mut off = [0usize];
        let out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut opt,
            block_word_off: &mut off,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });

        let expected_cinc = enc::sf(false, enc::csinc(X2, X6, X6, Cond::Ne));
        let expected_subs = enc::sf(false, enc::subs_imm(X2, X2, 1, false));
        let cmp_zero = enc::sf(false, enc::cmp_imm(X2, 0));
        let cmp_two = enc::sf(false, enc::cmp_imm(X2, 2));

        assert_eq!(out.stats.movs_eliminated, 1, "expected `cmp w2, #0` to be eliminated");
        assert_eq!(opt.len(), raw.len() - 1, "sequence must shrink by 1 instruction");
        let cinc_pos = opt.iter().position(|&w| w == expected_cinc);
        let subs_pos = opt.iter().position(|&w| w == expected_subs);
        assert!(cinc_pos.is_some(), "expected hoisted `cinc w2, w6, eq`");
        assert!(subs_pos.is_some(), "expected fused `subs w2, w2, #1`");
        assert!(
            cinc_pos < subs_pos,
            "hoisted `cinc w2, w6, eq` must precede `subs w2, w2, #1`"
        );
        assert!(
            !opt.iter().any(|&w| w == cmp_zero),
            "`cmp w2, #0` must be eliminated after `subs w2, w2, #1`"
        );
        assert!(
            opt.iter().any(|&w| w == cmp_two),
            "`cmp w2, #2` must be preserved"
        );

        // Hardware execution equivalence across modular boundary values (0, 1, 2, 3, u32::MAX)
        // for both `eq == true` and `eq == false`!
        // Signature: `extern "C" fn(x0_a: u64, x1_v: u64, x2_w: u64, x3_b: u64, x4_c: u64, x5_d: u64, x6_n: u64, x7_cnt: u64) -> u64`
        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let exec_raw = [
                enc::add_imm(X14, X0, 0, false),
                enc::add_imm(X15, X3, 0, false),
                enc::add_imm(X12, X4, 0, false),
                enc::sf(false, enc::adds(X11, X14, X2)),
                enc::sf(false, enc::sub_imm(X2, X6, 1, false)),
                enc::add_imm(X7, X7, 1, false),
                enc::sf(false, enc::csinc(X2, X2, X2, Cond::Ne)),
                enc::sf(false, enc::cmp_imm(X2, 0)),
                enc::csel(X14, X15, X14, Cond::Eq),
                enc::sf(false, enc::cmp_imm(X2, 2)),
                enc::csel(X12, X1, X12, Cond::Lo),
                enc::add(X0, X14, X12),
                enc::add(X0, X0, X2),
                enc::add(X0, X0, X7),
                enc::ret(),
            ];
            let orig_buf = words_vec(&exec_raw, alloc);
            let mut opt_buf = words_vec(&exec_raw, alloc);
            let mut exec_off = [0usize];
            let _ = optimize_mach_a64(&mut MachOptimizeInput {
                words: &mut opt_buf,
                block_word_off: &mut exec_off,
                block_order: &[0],
                patch_word_idxs: &mut [],
                patch_target_blocks: &[],
                reloc_byte_offs: &mut [],
                trap_byte_offs: &mut [],
                cold_stub_word_idxs: &[],
                extra_word_idxs: &mut [],
                align16_blocks: &[],
                volatile_word_idxs: &[],
                ret_live_gprs: RET_LIVE_GPRS,
                alloc,
            });
            assert!(!opt_buf.iter().any(|&w| w == cmp_zero));

            let ex_orig = exec_leaf(&orig_buf, alloc);
            let ex_opt = exec_leaf(&opt_buf, alloc);
            // SAFETY: Valid AAPCS64 leaf function touching only caller-saved GPRs `X0..=X15`.
            let f_orig: extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64) -> u64 = unsafe { ex_orig.as_fn() };
            // SAFETY: Same ABI-verified signature for optimized buffer.
            let f_opt: extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64) -> u64 = unsafe { ex_opt.as_fn() };

            for &n in &[0u64, 1, 2, 3, u64::from(u32::MAX)] {
                // `eq == true`: `0 + 0 == 0` sets Z=1; `eq == false`: `1 + 0 == 1` sets Z=0.
                for &(a, w) in &[(0u64, 0u64), (1u64, 0u64), (u64::from(u32::MAX), 1u64)] {
                    assert_eq!(
                        f_opt(a, 100, w, 200, 300, 0, n, 40),
                        f_orig(a, 100, w, 200, 300, 0, n, 40),
                        "mismatch for n={n}, a={a}, w={w}"
                    );
                }
            }
        }
    }

    #[test]
    fn peephole_sub1_cinc_cmp0_preserves_cmp_when_non_z_flag_read() {
        let alloc = test_alloc();

        // `csel ..., hi` reads the `C` flag (`C == 1 && Z == 0`). Since `subs w2, w2, #1` sets `C`
        // differently from `cmp w2, #0` (e.g. `C == 0` when `w2 == 0` before `- 1`), `cmp w2, #0`
        // MUST NOT be deleted — both when followed directly by `ret` and when followed by another `cmp`.
        for with_trailing_cmp in [false, true] {
            let mut raw = Vec::with_capacity_in(8, alloc);
            raw.push(enc::sf(false, enc::adds(X11, X14, X2)));
            raw.push(enc::sf(false, enc::sub_imm(X2, X6, 1, false)));
            raw.push(enc::add_imm(X7, X7, 1, false));
            raw.push(enc::sf(false, enc::csinc(X2, X2, X2, Cond::Ne)));
            raw.push(enc::sf(false, enc::cmp_imm(X2, 0)));
            raw.push(enc::csel(X14, X15, X14, Cond::Hi));
            if with_trailing_cmp {
                raw.push(enc::sf(false, enc::cmp_imm(X2, 2)));
            }
            raw.push(enc::ret());

            let orig_len = raw.len();
            let mut off = [0usize];
            let out = optimize_mach_a64(&mut MachOptimizeInput {
                words: &mut raw,
                block_word_off: &mut off,
                block_order: &[0],
                patch_word_idxs: &mut [],
                patch_target_blocks: &[],
                reloc_byte_offs: &mut [],
                trap_byte_offs: &mut [],
                cold_stub_word_idxs: &[],
                extra_word_idxs: &mut [],
                align16_blocks: &[],
                volatile_word_idxs: &[],
                ret_live_gprs: RET_LIVE_GPRS,
                alloc,
            });

            let cmp_zero = enc::sf(false, enc::cmp_imm(X2, 0));
            let subs_one = enc::sf(false, enc::subs_imm(X2, X2, 1, false));
            assert_eq!(out.stats.movs_eliminated, 0);
            assert_eq!(raw.len(), orig_len, "`cmp w2, #0` must not be deleted");
            assert!(
                raw.iter().any(|&w| w == cmp_zero),
                "`cmp w2, #0` feeding `csel ..., hi` (reads C flag) must be preserved"
            );
            assert!(
                !raw.iter().any(|&w| w == subs_one),
                "must not emit `subs w2, w2, #1` when `cmp w2, #0` is preserved"
            );
        }
    }

    #[test]
    fn peephole_sub1_cinc_preserves_order_when_rn_clobbered_between() {
        let alloc = test_alloc();

        // `add w6, w6, #1` between `sub w2, w6, #1` and `cinc w2, w2, eq` clobbers `w6` (`rn`),
        // so the pair MUST NOT be rewritten.
        let raw = [
            enc::sf(false, enc::adds(X11, X14, X2)),
            enc::sf(false, enc::sub_imm(X2, X6, 1, false)),
            enc::sf(false, enc::add_imm(X6, X6, 1, false)),
            enc::sf(false, enc::csinc(X2, X2, X2, Cond::Ne)),
            enc::sf(false, enc::cmp_imm(X2, 0)),
            enc::csel(X14, X15, X14, Cond::Eq),
            enc::sf(false, enc::cmp_imm(X2, 2)),
            enc::ret(),
        ];
        let mut opt = words_vec(&raw, alloc);
        let mut off = [0usize];
        let out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut opt,
            block_word_off: &mut off,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });

        let orig_sub = enc::sf(false, enc::sub_imm(X2, X6, 1, false));
        let orig_cinc = enc::sf(false, enc::csinc(X2, X2, X2, Cond::Ne));
        let rewritten_cinc = enc::sf(false, enc::csinc(X2, X6, X6, Cond::Ne));
        let rewritten_subs = enc::sf(false, enc::subs_imm(X2, X2, 1, false));
        let cmp_zero = enc::sf(false, enc::cmp_imm(X2, 0));

        assert_eq!(out.stats.movs_eliminated, 0);
        assert_eq!(opt.len(), raw.len());
        assert!(opt.iter().any(|&w| w == orig_sub), "`sub w2, w6, #1` must remain");
        assert!(opt.iter().any(|&w| w == orig_cinc), "`cinc w2, w2, eq` must remain");
        assert!(opt.iter().any(|&w| w == cmp_zero), "`cmp w2, #0` must remain");
        assert!(!opt.iter().any(|&w| w == rewritten_cinc), "must not rewrite to `cinc w2, w6, eq`");
        assert!(!opt.iter().any(|&w| w == rewritten_subs), "must not rewrite to `subs w2, w2, #1`");
    }

    #[test]
    fn peephole_sinks_sub_into_matching_cmp_across_flag_writes() {
        let alloc = test_alloc();

        // Mirrors `decode_exec_leaf_{single,wide}`: `sub w14, w6, #3` scheduled ahead of an
        // intervening `cmp w10, #2; csel ...`, followed by `cmp w6, #3; csel x0, x14, x10, hi`.
        // Must sink `sub w14, w6, #3` into `subs w14, w6, #3` at `cmp w6, #3` (-1 instruction!).
        let raw = [
            enc::sf(false, enc::sub_imm(X14, X6, 3, false)),
            enc::sf(false, enc::cmp_imm(X10, 2)),
            enc::csel(X10, X1, X2, Cond::Lo),
            enc::sf(false, enc::cmp_imm(X6, 3)),
            enc::csel(X0, X14, X10, Cond::Hi),
            enc::ret(),
        ];
        let mut opt = words_vec(&raw, alloc);
        let mut off = [0usize];
        let out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut opt,
            block_word_off: &mut off,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });

        let orig_sub = enc::sf(false, enc::sub_imm(X14, X6, 3, false));
        let orig_cmp = enc::sf(false, enc::cmp_imm(X6, 3));
        let fused_subs = enc::sf(false, enc::subs_imm(X14, X6, 3, false));

        assert_eq!(out.stats.movs_eliminated, 1);
        assert_eq!(opt.len(), raw.len() - 1);
        assert!(!opt.iter().any(|&w| w == orig_sub), "`sub w14, w6, #3` must be sunk");
        assert!(!opt.iter().any(|&w| w == orig_cmp), "`cmp w6, #3` must be replaced by `subs`");
        assert!(opt.iter().any(|&w| w == fused_subs), "expected fused `subs w14, w6, #3`");
    }

    #[test]
    fn peephole_fuses_lsr_ubfx_32_and_64_and_executes() {
        let alloc = test_alloc();

        // 1. 32-bit `lsr w1, w0, #8` + independent `add x2, x2, #1` + 64-bit `ubfx x1, x1, #0, #8`
        //    -> fuses into 32-bit `ubfm w1, w0, #8, #15` (`8 + 8 <= 32`) and deletes `lsr`.
        let raw32 = [
            enc::lsr_imm_w(X1, X0, 8),
            enc::add_imm(X2, X2, 1, false),
            enc::ubfx(true, X1, X1, 0, 8),
            enc::add(X0, X1, X2),
            enc::ret(),
        ];
        let mut opt32 = words_vec(&raw32, alloc);
        let mut off32 = [0usize];
        let out32 = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut opt32,
            block_word_off: &mut off32,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert_eq!(out32.stats.movs_eliminated, 1);
        assert_eq!(opt32.len(), raw32.len() - 1);
        assert!(!opt32.iter().any(|&w| w == enc::lsr_imm_w(X1, X0, 8)));
        assert!(
            opt32.iter().any(|&w| w == enc::ubfm_w(X1, X0, 8, 15)),
            "expected fused 32-bit `ubfx w1, w0, #8, #8`"
        );

        // 2. 64-bit `lsr x1, x0, #36` + 64-bit `ubfx x1, x1, #0, #12`
        //    -> fuses into 64-bit `ubfm x1, x0, #36, #47` (`36 + 12 = 48 > 32`).
        let raw64 = [
            enc::lsr_imm(X1, X0, 36),
            enc::ubfx(true, X1, X1, 0, 12),
            enc::add(X0, X1, X2),
            enc::ret(),
        ];
        let mut opt64 = words_vec(&raw64, alloc);
        let mut off64 = [0usize];
        let out64 = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut opt64,
            block_word_off: &mut off64,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert_eq!(out64.stats.movs_eliminated, 1);
        assert_eq!(opt64.len(), raw64.len() - 1);
        assert!(!opt64.iter().any(|&w| w == enc::lsr_imm(X1, X0, 36)));
        assert!(
            opt64.iter().any(|&w| w == enc::ubfm(X1, X0, 36, 47)),
            "expected fused 64-bit `ubfx x1, x0, #36, #12`"
        );

        // 3. Negative control: 32-bit `lsr w1, w0, #28` + `ubfx x1, x1, #0, #8` (`28 + 8 = 36 > 32`)
        //    must NOT fuse into `ubfm x1, x0, #28, #35` (would leak bits 35..32 of `x0`!).
        let raw_neg_ovf = [
            enc::lsr_imm_w(X1, X0, 28),
            enc::ubfx(true, X1, X1, 0, 8),
            enc::add(X0, X1, X2),
            enc::ret(),
        ];
        let mut opt_neg_ovf = words_vec(&raw_neg_ovf, alloc);
        let mut off_neg_ovf = [0usize];
        let out_neg_ovf = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut opt_neg_ovf,
            block_word_off: &mut off_neg_ovf,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert_eq!(out_neg_ovf.stats.movs_eliminated, 0);
        assert_eq!(opt_neg_ovf.len(), raw_neg_ovf.len());

        // 4. Negative control: intervening write to `rn` (`x0`) blocks fusion.
        let raw_neg_clob = [
            enc::lsr_imm(X1, X0, 8),
            enc::add_imm(X0, X0, 1, false),
            enc::ubfx(true, X1, X1, 0, 8),
            enc::add(X0, X0, X1),
            enc::ret(),
        ];
        let mut opt_neg_clob = words_vec(&raw_neg_clob, alloc);
        let mut off_neg_clob = [0usize];
        let out_neg_clob = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut opt_neg_clob,
            block_word_off: &mut off_neg_clob,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert_eq!(out_neg_clob.stats.movs_eliminated, 0);
        assert_eq!(opt_neg_clob.len(), raw_neg_clob.len());

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let ex32_raw = exec_leaf(&raw32, alloc);
            let ex32_opt = exec_leaf(&opt32, alloc);
            let ex64_raw = exec_leaf(&raw64, alloc);
            let ex64_opt = exec_leaf(&opt64, alloc);
            // SAFETY: Valid AAPCS64 leaf functions touching only caller-saved X0..X2.
            let f32_raw: extern "C" fn(u64, u64, u64) -> u64 = unsafe { ex32_raw.as_fn() };
            // SAFETY: Same verified signature.
            let f32_opt: extern "C" fn(u64, u64, u64) -> u64 = unsafe { ex32_opt.as_fn() };
            // SAFETY: Same verified signature.
            let f64_raw: extern "C" fn(u64, u64, u64) -> u64 = unsafe { ex64_raw.as_fn() };
            // SAFETY: Same verified signature.
            let f64_opt: extern "C" fn(u64, u64, u64) -> u64 = unsafe { ex64_opt.as_fn() };

            for &(v, k) in &[
                (0xdead_beef_cafe_babeu64, 7u64),
                (0xffff_ffff_1234_5678u64, 19u64),
                (0x0123_4567_89ab_cdefu64, 0u64),
                (0u64, 42u64),
            ] {
                assert_eq!(f32_opt(v, 0, k), f32_raw(v, 0, k), "32-bit lsr+ubfx mismatch");
                assert_eq!(f64_opt(v, 0, k), f64_raw(v, 0, k), "64-bit lsr+ubfx mismatch");
            }
        }
    }

    #[test]
    fn peephole_elides_dead_narrow_ubfx_for_stores_and_orr_with_negative_controls() {
        let alloc = test_alloc();
        let orr_w_lsl = |rd: u32, rn: u32, rm: u32, shift: u32| -> u32 {
            0x2a00_0000 | (rm << 16) | ((shift & 31) << 10) | (rn << 5) | rd
        };

        // 1. Cross-block dead `ubfx x4, x4, #0, #16` before `strh w4, [x14, x1, lsl #1]`:
        //    `x4` is NOT rewritten before `b.ne`, but IS overwritten at loop header top and dead on exit!
        let mut loop_words = words_vec(
            &[
                // Block 0 (loop body):
                enc::add_imm(X4, X2, 1, false),
                enc::ubfx(true, X4, X4, 0, 16),
                enc::str_reg_scaled(Size::H, X4, X14, X1),
                enc::subs_imm(X2, X2, 1, false),
                enc::b_cond(Cond::Ne, 0),
                // Block 1 (exit):
                enc::mov(X0, X2),
                enc::movz(X4, 0, 0),
                enc::ret(),
            ],
            alloc,
        );
        let mut loop_off = [0usize, 5];
        let mut loop_patches = [4usize];
        let loop_targets = [0u32];
        let loop_out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut loop_words,
            block_word_off: &mut loop_off,
            block_order: &[0, 1],
            patch_word_idxs: &mut loop_patches,
            patch_target_blocks: &loop_targets,
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert_eq!(loop_out.stats.movs_eliminated, 1);
        assert!(
            !loop_words.iter().any(|&w| w == enc::ubfx(true, X4, X4, 0, 16)),
            "dead narrow `ubfx x4, x4, #0, #16` before `strh` must be elided"
        );
        assert!(
            loop_words.iter().any(|&w| w == enc::str_reg_scaled(Size::H, X4, X14, X1)),
            "`strh w4, [x14, x1, lsl #1]` must be preserved"
        );

        // 2. `rd != rn` narrow `ubfx #0, #8` before `strb`, `orr w, w, w_rd, lsl #24`, and `lsl w, w_rd, #24`:
        //    all three `ubfx` instructions must be deleted and uses rewritten from `rd` -> `rn`.
        let raw_narrow = [
            enc::ubfx(true, X8, X0, 0, 8),
            enc::str(Size::B, X8, X1, 0),
            enc::ubfx(true, X9, X3, 0, 8),
            orr_w_lsl(X2, X2, X9, 24),
            enc::ubfx(false, X10, X4, 0, 8),
            enc::lsl_imm_w(X5, X10, 24),
            enc::sf(false, enc::add(X0, X2, X5)),
            enc::ret(),
        ];
        let mut opt_narrow = words_vec(&raw_narrow, alloc);
        let mut off_narrow = [0usize];
        let out_narrow = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut opt_narrow,
            block_word_off: &mut off_narrow,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert_eq!(out_narrow.stats.movs_eliminated, 3);
        assert_eq!(opt_narrow.len(), raw_narrow.len() - 3);
        assert!(opt_narrow.iter().any(|&w| w == enc::str(Size::B, X0, X1, 0)));
        assert!(opt_narrow.iter().any(|&w| w == orr_w_lsl(X2, X2, X3, 24)));
        assert!(opt_narrow.iter().any(|&w| w == enc::lsl_imm_w(X5, X4, 24)));

        // 3. Negative control: `rd` (`x0`) is live-out at `ret` -> MUST NOT delete `ubfx`.
        let raw_live = [enc::ubfx(true, X0, X0, 0, 16), enc::str(Size::H, X0, X1, 0), enc::ret()];
        let mut opt_live = words_vec(&raw_live, alloc);
        let mut off_live = [0usize];
        let out_live = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut opt_live,
            block_word_off: &mut off_live,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert_eq!(out_live.stats.movs_eliminated, 0);
        assert_eq!(opt_live.len(), raw_live.len());

        // 4. Negative control: `rd` (`x8`) is also read in a wide `add` -> MUST NOT delete `ubfx`.
        let raw_wide = [
            enc::ubfx(true, X8, X0, 0, 8),
            enc::str(Size::B, X8, X1, 0),
            enc::add(X0, X2, X8),
            enc::ret(),
        ];
        let mut opt_wide = words_vec(&raw_wide, alloc);
        let mut off_wide = [0usize];
        let out_wide = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut opt_wide,
            block_word_off: &mut off_wide,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert_eq!(out_wide.stats.movs_eliminated, 0);
        assert_eq!(opt_wide.len(), raw_wide.len());

        // 5. Negative control: `rd` (`x8`) is also the index register of `strh` -> MUST NOT delete `ubfx`.
        let raw_idx = [
            enc::ubfx(true, X8, X0, 0, 16),
            enc::str_reg_scaled(Size::H, X8, X1, X8),
            enc::ret(),
        ];
        let mut opt_idx = words_vec(&raw_idx, alloc);
        let mut off_idx = [0usize];
        let out_idx = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut opt_idx,
            block_word_off: &mut off_idx,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert_eq!(out_idx.stats.movs_eliminated, 0);
        assert_eq!(opt_idx.len(), raw_idx.len());

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let ex_raw = exec_leaf(&raw_narrow, alloc);
            let ex_opt = exec_leaf(&opt_narrow, alloc);
            // SAFETY: Valid AAPCS64 leaf touching only caller-saved X0..X10 and writing 1 byte at X1.
            let f_raw: extern "C" fn(u64, *mut u8, u64, u64, u64) -> u64 = unsafe { ex_raw.as_fn() };
            // SAFETY: Same verified signature.
            let f_opt: extern "C" fn(u64, *mut u8, u64, u64, u64) -> u64 = unsafe { ex_opt.as_fn() };

            for &(a, b, c, d) in &[
                (0x1234_5678_9abc_def1u64, 0x0011_2233u64, 0xdead_be77u64, 0xcafe_ba42u64),
                (0xffff_ff00u64, 0x00ab_cd00u64, 0x5555_55ffu64, 0x8000_0001u64),
            ] {
                let mut byte_raw = 0u8;
                let mut byte_opt = 0u8;
                let r_raw = f_raw(a, &raw mut byte_raw, b, c, d);
                let r_opt = f_opt(a, &raw mut byte_opt, b, c, d);
                assert_eq!(r_opt, r_raw, "narrow ubfx return mismatch");
                assert_eq!(byte_opt, byte_raw, "narrow ubfx stored byte mismatch");
            }
        }
    }

    #[test]
    fn mach_branch_opt_tail_dups_trampoline_elides_both_branches_and_folds_movz_lsr() {
        let alloc = test_alloc();

        // Layout:
        //   Block 0 (`P`): `movz x15, #0x40; b Block 2 (T)`
        //   Block 1 (`F`): `add x0, x15, x10; ret`          (immediately follows `P`!)
        //   Block 2 (`T`): `lsr x10, x15, #6; cmp x10, x1; b.hi Block 3; b Block 1`
        //   Block 3 (`exit`, align16): `movz x0, #99; ret`
        //
        // Duplicating `T` into `P` must:
        //   - elide `P`'s `b T` AND the duplicated trailing `b F` (`branches_elided >= 2`),
        //   - fold `movz x15, #0x40; lsr x10, x15, #6` into `movz x15, #0x40; movz x10, #1`,
        //   - keep `P`'s duplicated `b.hi Block 3` accurately patched across `align16` padding!
        let orig_template = [
            // Block 0 (word 0..2):
            enc::movz(X15, 0x40, 0),
            enc::b(0), // patch 0 -> Block 2
            // Block 1 (word 2..4):
            enc::add(X0, X15, X10),
            enc::ret(),
            // Block 2 (word 4..8):
            enc::lsr_imm(X10, X15, 6),
            enc::cmp(X10, X1),
            enc::b_cond(Cond::Hi, 0), // patch 1 -> Block 3
            enc::b(0),                // patch 2 -> Block 1
            // Block 3 (word 8..10):
            enc::movz(X0, 99, 0),
            enc::ret(),
        ];
        let mut orig = words_vec(&orig_template, alloc);
        let orig_off = [0usize, 2, 4, 8];
        let orig_patches = [1usize, 6, 7];
        let patch_targets = [2u32, 3, 1];
        patch_branches(&mut orig, &orig_patches, &patch_targets, &orig_off);

        let mut opt = words_vec(&orig_template, alloc);
        let mut opt_off = orig_off;
        let mut opt_patches = orig_patches;
        let align16_blocks = [false, false, false, true];
        let out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut opt,
            block_word_off: &mut opt_off,
            block_order: &[0, 1, 2, 3],
            patch_word_idxs: &mut opt_patches,
            patch_target_blocks: &patch_targets,
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &align16_blocks,
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        patch_branches(&mut opt, &opt_patches, &patch_targets, &opt_off);

        assert!(
            out.stats.branches_elided >= 2,
            "expected BOTH `b T` and fallthrough `b F` to be elided, got {}",
            out.stats.branches_elided
        );
        assert_eq!(opt_patches[0], ELIDED_PATCH, "P's `b T` patch must be elided");
        assert_eq!(opt_off[3] % 4, 0, "aligned exit block must stay 16-byte aligned");

        // Block 0 (`opt[opt_off[0]..opt_off[1]]`) must be:
        //   `movz x15, #0x40`, `movz x10, #1`, `cmp x10, x1`, `b.hi Block 3` (no `lsr`, no `b`!).
        let p_slice = &opt[opt_off[0]..opt_off[1]];
        assert_eq!(p_slice.len(), 4, "expected 4 words in tail-duplicated P, got {p_slice:?}");
        assert_eq!(p_slice[0], enc::movz(X15, 0x40, 0));
        assert_eq!(p_slice[1], enc::movz(X10, 1, 0), "expected folded `movz x10, #1`");
        assert_eq!(p_slice[2], enc::cmp(X10, X1));
        assert!(
            matches!(decode_inst(p_slice[3]).kind, DecodedKind::BranchCond { is_flag_cond: true, .. }),
            "expected pre-resolved `b.hi` at end of P"
        );
        assert!(
            !p_slice.iter().any(|&w| matches!(decode_inst(w).kind, DecodedKind::BranchUncond { .. })),
            "P must have zero unconditional branches after tail-dup + fallthrough elision"
        );

        // Negative control: trampoline containing a `Call` (`bl`) must NOT be duplicated.
        let neg_template = [
            enc::movz(X15, 0x40, 0),
            enc::b(0), // patch 0 -> Block 2
            enc::add(X0, X15, X10),
            enc::ret(),
            enc::bl(0), // call inside T!
            enc::b(0),  // patch 1 -> Block 1
        ];
        let mut neg = words_vec(&neg_template, alloc);
        let mut neg_off = [0usize, 2, 4];
        let mut neg_patches = [1usize, 5];
        let neg_targets = [2u32, 1];
        let neg_out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut neg,
            block_word_off: &mut neg_off,
            block_order: &[0, 1, 2],
            patch_word_idxs: &mut neg_patches,
            patch_target_blocks: &neg_targets,
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert_eq!(neg_out.stats.branches_elided, 0);
        assert_eq!(neg_patches[0], 1, "trampoline containing a call must not be tail-duplicated");

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let ex_orig = exec_leaf(&orig, alloc);
            let ex_opt = exec_leaf(&opt, alloc);
            // SAFETY: Valid AAPCS64 leaf functions touching only caller-saved X0, X1, X10, X15.
            let f_orig: extern "C" fn(u64, u64) -> u64 = unsafe { ex_orig.as_fn() };
            // SAFETY: Same verified signature.
            let f_opt: extern "C" fn(u64, u64) -> u64 = unsafe { ex_opt.as_fn() };

            // `x1 = 0`: `1 > 0` takes `b.hi Block 3` -> returns 99.
            assert_eq!(f_opt(0, 0), 99);
            assert_eq!(f_opt(0, 0), f_orig(0, 0));
            // `x1 = 1` / `5`: `1 <= x1` falls straight into Block 1 (`0x40 + 1 = 65`).
            assert_eq!(f_opt(0, 1), 65);
            assert_eq!(f_opt(0, 1), f_orig(0, 1));
            assert_eq!(f_opt(0, 5), 65);
            assert_eq!(f_opt(0, 5), f_orig(0, 5));
        }
    }

    #[test]
    fn exact_ret_live_gprs_elides_dead_narrow_ubfx_on_non_return_gprs() {
        let alloc = test_alloc();
        // Models 1-return/void functions (e.g. `FseDecTable::build_from_norm_inner`) where
        // narrow `ubfx x4, x4, #0, #16` and `ubfx x1, x1, #0, #8` feed narrow stores before `ret`.
        let raw = [
            enc::ubfx(true, X4, X4, 0, 16),
            enc::str(Size::H, X4, X2, 0),
            enc::ubfx(true, X1, X1, 0, 8),
            enc::str(Size::B, X1, X3, 0),
            enc::ubfx(true, X0, X0, 0, 16),
            enc::str(Size::H, X0, X5, 0),
            enc::ret(),
        ];

        // 1. Conservative `RET_LIVE_GPRS` (`x0..=x7` live): keeps all 3 `ubfx`.
        let mut cons = words_vec(&raw, alloc);
        let mut cons_off = [0usize];
        let cons_out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut cons,
            block_word_off: &mut cons_off,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert_eq!(cons_out.stats.movs_eliminated, 0);
        assert_eq!(cons.len(), raw.len());

        // 2. Exact 1-GPR return (`X0` live only): elides `ubfx x4` and `ubfx x1`, keeps `ubfx x0`.
        let mut ret1 = words_vec(&raw, alloc);
        let mut ret1_off = [0usize];
        let ret1_out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut ret1,
            block_word_off: &mut ret1_off,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_CALLEE_SAVED_GPRS | (1u32 << X0),
            alloc,
        });
        assert_eq!(ret1_out.stats.movs_eliminated, 2);
        assert!(!ret1.iter().any(|&w| w == enc::ubfx(true, X4, X4, 0, 16)));
        assert!(!ret1.iter().any(|&w| w == enc::ubfx(true, X1, X1, 0, 8)));
        assert!(ret1.iter().any(|&w| w == enc::ubfx(true, X0, X0, 0, 16)));

        // 3. Void return (`RET_CALLEE_SAVED_GPRS` only): elides all 3 `ubfx`.
        let mut ret0 = words_vec(&raw, alloc);
        let mut ret0_off = [0usize];
        let ret0_out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut ret0,
            block_word_off: &mut ret0_off,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_CALLEE_SAVED_GPRS,
            alloc,
        });
        assert_eq!(ret0_out.stats.movs_eliminated, 3);
        assert_eq!(ret0.len(), raw.len() - 3);

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let ex_raw = exec_leaf(&raw, alloc);
            let ex_opt = exec_leaf(&ret1, alloc);
            // SAFETY: Valid AAPCS64 leaf writing narrow halfword/byte outputs through X2/X3/X5 and returning X0.
            let f_raw: extern "C" fn(u64, u64, *mut u16, *mut u8, u64, *mut u16) -> u64 = unsafe { ex_raw.as_fn() };
            // SAFETY: Same verified signature.
            let f_opt: extern "C" fn(u64, u64, *mut u16, *mut u8, u64, *mut u16) -> u64 = unsafe { ex_opt.as_fn() };

            let (mut h4_raw, mut b1_raw, mut h0_raw) = (0u16, 0u8, 0u16);
            let (mut h4_opt, mut b1_opt, mut h0_opt) = (0u16, 0u8, 0u16);
            let r_raw = f_raw(
                0xdead_beef_1234_5678,
                0x1122_3344_5566_77ab,
                &raw mut h4_raw,
                &raw mut b1_raw,
                0x9988_7766_5544_cafe,
                &raw mut h0_raw,
            );
            let r_opt = f_opt(
                0xdead_beef_1234_5678,
                0x1122_3344_5566_77ab,
                &raw mut h4_opt,
                &raw mut b1_opt,
                0x9988_7766_5544_cafe,
                &raw mut h0_opt,
            );
            assert_eq!(r_opt, r_raw);
            assert_eq!(r_opt, 0x5678);
            assert_eq!(h4_opt, h4_raw);
            assert_eq!(b1_opt, b1_raw);
            assert_eq!(h0_opt, h0_raw);
        }
    }

    #[test]
    fn mach_branch_opt_tail_dups_trampoline_into_cold_stub_and_folds_lsr() {
        let alloc = test_alloc();

        // Predecessor `P` is a cold stub AFTER the last LIR block (`block_word_off` does NOT
        // include `P`, `cold_stub_word_idxs` is empty, and ONLY an internal branch `cbz x3, P`
        // targets `P`!):
        //   Block 0 (`entry`, 0..7): `stp x19, x27, [sp, #-16]!; cbz x3, P; add x15, x3, #1 (x4); b T`
        //   Block 1 (`header`, 7..10, align16): `add x0, x19, x27; ldp x19, x27, [sp], #16; ret`
        //   Block 2 (`T`, 10..16): `lsr x27, x15, #6; add x0, x2, x27; cmp x0, x7; b.hi exit; mov x19, x15; b header`
        //   Block 3 (`exit`, 16..19): `movz x0, #99; ldp x19, x27, [sp], #16; ret`
        //   Cold stub `P` (19..21): `movz x15, #0x40; b T`
        let orig_template = [
            // Block 0 (words 0..7):
            enc::stp_pre(X19, X27, SP, -16),
            enc::cbz(X3, 18), // [1] internal branch -> word 19 (cold stub P)
            enc::add_imm(X15, X3, 1, false),
            enc::add_imm(X15, X15, 1, false),
            enc::add_imm(X15, X15, 1, false),
            enc::add_imm(X15, X15, 1, false),
            enc::b(0), // [6] patch 0 -> Block 2 (T)
            // Block 1 (`header`, words 7..10):
            enc::add(X0, X19, X27),
            enc::ldp_post(X19, X27, SP, 16),
            enc::ret(),
            // Block 2 (`T`, words 10..16):
            enc::lsr_imm(X27, X15, 6),
            enc::add(X0, X2, X27),
            enc::cmp(X0, X7),
            enc::b_cond(Cond::Hi, 0), // [13] patch 1 -> Block 3 (exit)
            enc::mov(X19, X15),
            enc::b(0), // [15] patch 2 -> Block 1 (header)
            // Block 3 (`exit`, words 16..19):
            enc::movz(X0, 99, 0),
            enc::ldp_post(X19, X27, SP, 16),
            enc::ret(),
            // Cold stub `P` (words 19..21, AFTER last LIR block):
            enc::movz(X15, 0x40, 0),
            enc::b(0), // [20] patch 3 -> Block 2 (T)
        ];
        let mut orig = words_vec(&orig_template, alloc);
        let orig_off = [0usize, 7, 10, 16];
        let orig_patches = [6usize, 13, 15, 20];
        let patch_targets = [2u32, 3, 1, 2];
        patch_branches(&mut orig, &orig_patches, &patch_targets, &orig_off);

        let mut opt = words_vec(&orig_template, alloc);
        let mut opt_off = orig_off;
        let mut opt_patches = orig_patches;
        let align16_blocks = [false, true, false, false];
        let out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut opt,
            block_word_off: &mut opt_off,
            block_order: &[0, 1, 2, 3],
            patch_word_idxs: &mut opt_patches,
            patch_target_blocks: &patch_targets,
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &align16_blocks,
            volatile_word_idxs: &[],
            ret_live_gprs: RET_CALLEE_SAVED_GPRS | (1u32 << X0),
            alloc,
        });
        patch_branches(&mut opt, &opt_patches, &patch_targets, &opt_off);

        assert!(
            out.stats.branches_elided >= 1,
            "cold stub P's `b T` must be elided by trampoline tail-dup, got {}",
            out.stats.branches_elided
        );
        assert_eq!(opt_patches[3], ELIDED_PATCH, "cold stub P's `b T` patch must be elided");
        assert_eq!(opt_off[1] % 4, 0, "aligned header block must stay 16-byte aligned");

        // Cold stub `P` begins immediately after Block 3's 3 instructions (`opt_off[3] + 3`).
        let p_start = opt_off[3] + 3;
        let p_slice = &opt[p_start..];
        assert_eq!(p_slice.len(), 7, "expected 7 words in tail-duplicated cold stub P, got {p_slice:#x?}");
        assert_eq!(p_slice[0], enc::movz(X15, 0x40, 0));
        assert_eq!(p_slice[1], enc::movz(X27, 1, 0), "expected folded `movz x27, #1` (from `lsr x27, x15, #6`)");
        assert_eq!(p_slice[2], enc::add(X0, X2, X27));
        assert_eq!(p_slice[3], enc::cmp(X0, X7));
        assert_eq!(p_slice[5], enc::mov(X19, X15));

        // Verify `cbz x3, P` still points to `p_start`, `b.hi` targets `opt_off[3]` (`exit`),
        // and trailing `b` targets `opt_off[1]` (`header`) — NOT `opt_off[2]` (`T`)!
        let DecodedKind::BranchCond { delta_words: cbz_delta, is_flag_cond: false, .. } = decode_inst(opt[1]).kind else {
            panic!("expected `cbz x3, P` at word 1, got {:#x}", opt[1]);
        };
        assert_eq!(
            usize::try_from(1i64 + i64::from(cbz_delta)).ok(),
            Some(p_start),
            "internal `cbz x3, P` must track remapped cold stub start"
        );
        let DecodedKind::BranchCond { delta_words: bhi_delta, is_flag_cond: true, cond: 8 } =
            decode_inst(p_slice[4]).kind
        else {
            panic!("expected `b.hi exit` at p_slice[4], got {:#x}", p_slice[4]);
        };
        let bhi_idx = i64::try_from(p_start + 4).unwrap_or(0);
        assert_eq!(
            usize::try_from(bhi_idx + i64::from(bhi_delta)).ok(),
            Some(opt_off[3]),
            "duplicated `b.hi` in cold stub P must target `exit`"
        );
        let DecodedKind::BranchUncond { delta_words: bhdr_delta } = decode_inst(p_slice[6]).kind else {
            panic!("expected `b header` at p_slice[6], got {:#x}", p_slice[6]);
        };
        let bhdr_idx = i64::try_from(p_start + 6).unwrap_or(0);
        assert_eq!(
            usize::try_from(bhdr_idx + i64::from(bhdr_delta)).ok(),
            Some(opt_off[1]),
            "duplicated `b` in cold stub P must target `header` (not `T`)"
        );

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let ex_orig = exec_leaf(&orig, alloc);
            let ex_opt = exec_leaf(&opt, alloc);
            // SAFETY: Valid AAPCS64 function saving/restoring callee-saved X19/X27 on stack.
            let f_orig: extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64) -> u64 = unsafe { ex_orig.as_fn() };
            // SAFETY: Same verified signature.
            let f_opt: extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64) -> u64 = unsafe { ex_opt.as_fn() };

            for &(x2, x3, x7, expected) in &[
                (10u64, 0u64, 10u64, 99u64),   // cold stub P -> 10 + 1 > 10 -> exit (99)
                (10u64, 0u64, 11u64, 65u64),   // cold stub P -> 10 + 1 <= 11 -> header (0x40 + 1 = 65)
                (10u64, 124u64, 11u64, 99u64), // non-cold -> x15=128, x27=2, 10 + 2 > 11 -> exit (99)
                (10u64, 124u64, 12u64, 130u64), // non-cold -> x15=128, x27=2, 10 + 2 <= 12 -> header (130)
            ] {
                assert_eq!(f_opt(0, 0, x2, x3, 0, 0, 0, x7), expected);
                assert_eq!(f_opt(0, 0, x2, x3, 0, 0, 0, x7), f_orig(0, 0, x2, x3, 0, 0, 0, x7));
            }
        }
    }

    #[test]
    fn mach_redundant_loads_unlocks_ldp_pairing_and_forwards_w32_and_x64() {
        let alloc = test_alloc();

        // 1. Duplicate base-pointer spill reload:
        //    `ldr x17, [sp]; ldr x8, [x17, #0x80]; ldr x17, [sp]; ldr x2, [x17, #0x88]`
        //    -> Pass 1b deletes the second `ldr x17, [sp]`, immediately unlocking Pass 2 to fuse
        //       `ldr x8, [x17, #0x80]` + `ldr x2, [x17, #0x88]` into `ldp x8, x2, [x17, #0x80]`!
        let pair_raw = [
            enc::sub_imm(SP, SP, 16, false),
            enc::str(Size::X, X0, SP, 0),
            enc::movz(X0, 0, 0),
            enc::ldr(Size::X, X17, SP, 0),
            enc::ldr(Size::X, X8, X17, 0x10), // [x17, #0x80]
            enc::ldr(Size::X, X17, SP, 0),    // duplicate spill reload -> DELETED!
            enc::ldr(Size::X, X2, X17, 0x11), // [x17, #0x88] -> fuses with #0x80 into ldp!
            enc::add(X0, X8, X2),
            enc::add_imm(SP, SP, 16, false),
            enc::ret(),
        ];
        let mut pair_opt = words_vec(&pair_raw, alloc);
        let mut pair_off = [0usize];
        let pair_out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut pair_opt,
            block_word_off: &mut pair_off,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert_eq!(pair_out.stats.movs_eliminated, 1, "second `ldr x17, [sp]` must be deleted");
        assert_eq!(pair_out.stats.pairs_fused, 1, "deleting duplicate `ldr x17, [sp]` must unlock `ldp` pairing");
        assert_eq!(pair_opt.len(), pair_raw.len() - 2);
        assert_eq!(
            pair_opt.iter().filter(|&&w| w == enc::ldr(Size::X, X17, SP, 0)).count(),
            1,
            "only the first `ldr x17, [sp]` should remain"
        );
        assert!(
            pair_opt.iter().any(|&w| w == enc::ldp_off(X8, X2, X17, 0x80)),
            "expected fused `ldp x8, x2, [x17, #0x80]`"
        );

        // 2. Immediate `str x16, [sp, #0x28]; ldr x16, [sp, #0x28]` -> `ldr` is deleted!
        let stld_x64_raw = [
            enc::sub_imm(SP, SP, 64, false),
            enc::add_imm(X16, X0, 3, false),
            enc::str(Size::X, X16, SP, 5), // [sp, #0x28]
            enc::ldr(Size::X, X16, SP, 5), // [sp, #0x28] -> DELETED!
            enc::add(X0, X16, X1),
            enc::add_imm(SP, SP, 64, false),
            enc::ret(),
        ];
        let mut stld_x64_opt = words_vec(&stld_x64_raw, alloc);
        let mut stld_x64_off = [0usize];
        let stld_x64_out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut stld_x64_opt,
            block_word_off: &mut stld_x64_off,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert!(stld_x64_out.stats.movs_eliminated >= 1);
        assert!(
            !stld_x64_opt.iter().any(|&w| w == enc::ldr(Size::X, X16, SP, 5)),
            "immediate `ldr x16, [sp, #0x28]` after `str x16, [sp, #0x28]` must be deleted"
        );

        // 3. Immediate `str w9, [sp, #0x28]; ldr w9, [sp, #0x28]` -> replaced with 32-bit
        //    `mov w9, w9` (preserving zero-extension of upper 32 bits when `x9` had dirty upper bits!).
        let stld_w32_raw = [
            enc::sub_imm(SP, SP, 64, false),
            enc::add_imm(X9, X0, 0, false), // x9 receives full 64-bit arg (dirty upper 32 bits!)
            enc::str(Size::W, X9, SP, 10),  // [sp, #0x28]
            enc::ldr(Size::W, X9, SP, 10),  // [sp, #0x28] -> `mov w9, w9`!
            enc::add(X0, X9, X1),           // observes full 64-bit x9 (upper 32 bits MUST be 0!)
            enc::add_imm(SP, SP, 64, false),
            enc::ret(),
        ];
        let mut stld_w32_opt = words_vec(&stld_w32_raw, alloc);
        let mut stld_w32_off = [0usize];
        let stld_w32_out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut stld_w32_opt,
            block_word_off: &mut stld_w32_off,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert_eq!(stld_w32_out.stats.movs_eliminated, 1);
        assert!(
            !stld_w32_opt.iter().any(|&w| w == enc::ldr(Size::W, X9, SP, 10)),
            "`ldr w9, [sp, #0x28]` must be replaced with 32-bit `mov w9, w9`"
        );
        assert!(
            stld_w32_opt.iter().any(|&w| w == enc::sf(false, enc::mov(X9, X9))),
            "expected 32-bit zero-extending `mov w9, w9`"
        );

        // 4. Non-overlapping SP store: `ldr x17, [sp, #0x10]; str x1, [sp, #0x18]; ldr x17, [sp, #0x10]`
        //    -> second `ldr` IS deleted because `[sp+0x18..0x20)` is disjoint from `[sp+0x10..0x18)`!
        let disjoint_raw = [
            enc::sub_imm(SP, SP, 32, false),
            enc::str(Size::X, X0, SP, 2),  // [sp, #0x10]
            enc::movz(X0, 0, 0),
            enc::ldr(Size::X, X17, SP, 2), // [sp, #0x10..#0x18)
            enc::str(Size::X, X1, SP, 3),  // [sp, #0x18..#0x20) — disjoint!
            enc::ldr(Size::X, X17, SP, 2), // REDUNDANT -> DELETED!
            enc::add(X0, X17, X1),
            enc::add_imm(SP, SP, 32, false),
            enc::ret(),
        ];
        let mut disjoint_opt = words_vec(&disjoint_raw, alloc);
        let mut disjoint_off = [0usize];
        let disjoint_out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut disjoint_opt,
            block_word_off: &mut disjoint_off,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert_eq!(disjoint_out.stats.movs_eliminated, 1);
        assert_eq!(
            disjoint_opt.iter().filter(|&&w| w == enc::ldr(Size::X, X17, SP, 2)).count(),
            1,
            "second `ldr x17, [sp, #0x10]` across disjoint `str x1, [sp, #0x18]` must be deleted"
        );

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let ex_pair_orig = exec_leaf(&pair_raw, alloc);
            let ex_pair_opt = exec_leaf(&pair_opt, alloc);
            // SAFETY: Valid AAPCS64 leaf function reading `[x0 + 0x80]` and `[x0 + 0x88]`.
            let f_pair_orig: extern "C" fn(*const u64) -> u64 = unsafe { ex_pair_orig.as_fn() };
            // SAFETY: Same verified signature.
            let f_pair_opt: extern "C" fn(*const u64) -> u64 = unsafe { ex_pair_opt.as_fn() };
            let mut buf = [0u64; 18];
            buf[16] = 0x1122_3344_5566_7788;
            buf[17] = 0x0102_0304_0506_0708;
            assert_eq!(f_pair_opt(buf.as_ptr()), f_pair_orig(buf.as_ptr()));
            assert_eq!(f_pair_opt(buf.as_ptr()), buf[16].wrapping_add(buf[17]));

            let ex_w32_orig = exec_leaf(&stld_w32_raw, alloc);
            let ex_w32_opt = exec_leaf(&stld_w32_opt, alloc);
            // SAFETY: Valid AAPCS64 leaf function touching only caller-saved GPRs and 64B stack frame.
            let f_w32_orig: extern "C" fn(u64, u64) -> u64 = unsafe { ex_w32_orig.as_fn() };
            // SAFETY: Same verified signature.
            let f_w32_opt: extern "C" fn(u64, u64) -> u64 = unsafe { ex_w32_opt.as_fn() };
            for &(x0v, x1v) in &[
                (0xdead_beef_1234_5678u64, 0x10u64),
                (0xffff_ffff_ffff_ffffu64, 1u64),
                (0x8000_0000_0000_0005u64, 7u64),
            ] {
                let got = f_w32_opt(x0v, x1v);
                assert_eq!(got, f_w32_orig(x0v, x1v));
                assert_eq!(got, (x0v & 0xffff_ffff).wrapping_add(x1v));
            }

            let ex_disj_orig = exec_leaf(&disjoint_raw, alloc);
            let ex_disj_opt = exec_leaf(&disjoint_opt, alloc);
            // SAFETY: Valid AAPCS64 leaf function touching only caller-saved GPRs and 32B stack frame.
            let f_disj_orig: extern "C" fn(u64, u64) -> u64 = unsafe { ex_disj_orig.as_fn() };
            // SAFETY: Same verified signature.
            let f_disj_opt: extern "C" fn(u64, u64) -> u64 = unsafe { ex_disj_opt.as_fn() };
            assert_eq!(f_disj_opt(111, 222), f_disj_orig(111, 222));
            assert_eq!(f_disj_opt(111, 222), 333);
        }
    }

    #[test]
    fn mach_redundant_loads_soundness_negative_controls() {
        let alloc = test_alloc();

        // 1. `str xzr, [sp, #0x10]; ldr x9, [sp, #0x10]` -> MUST NOT replace with `mov x9, sp` (`rt == 31`)!
        let xzr_raw = [
            enc::sub_imm(SP, SP, 32, false),
            enc::str(Size::X, enc::ZR, SP, 2),
            enc::ldr(Size::X, X9, SP, 2),
            enc::add(X0, X9, X1),
            enc::add_imm(SP, SP, 32, false),
            enc::ret(),
        ];
        let mut xzr_opt = words_vec(&xzr_raw, alloc);
        let mut xzr_off = [0usize];
        let xzr_out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut xzr_opt,
            block_word_off: &mut xzr_off,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert_eq!(xzr_out.stats.movs_eliminated, 0);
        assert!(
            xzr_opt.iter().any(|&w| w == enc::ldr(Size::X, X9, SP, 2)),
            "`ldr x9, [sp, #0x10]` after `str xzr, [sp, #0x10]` must be preserved"
        );
        assert!(
            !xzr_opt.iter().any(|&w| w == enc::mov(X9, SP)),
            "must never replace `str xzr` + `ldr` with `mov x9, sp`"
        );

        // 2. Aliasing non-SP store: `ldr x17, [sp, #0x10]; str x1, [x2, #0]; ldr x17, [sp, #0x10]`
        //    -> second `ldr` MUST be preserved (`x2` may alias `sp + 0x10`)!
        let alias_raw = [
            enc::sub_imm(SP, SP, 32, false),
            enc::str(Size::X, X0, SP, 2),
            enc::movz(X0, 0, 0),
            enc::add_imm(X2, SP, 16, false), // x2 == sp + 0x10!
            enc::ldr(Size::X, X17, SP, 2),
            enc::str(Size::X, X1, X2, 0),    // aliases [sp, #0x10]!
            enc::ldr(Size::X, X17, SP, 2),   // MUST be preserved!
            enc::add(X0, X17, X0),
            enc::add_imm(SP, SP, 32, false),
            enc::ret(),
        ];
        let mut alias_opt = words_vec(&alias_raw, alloc);
        let mut alias_off = [0usize];
        let alias_out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut alias_opt,
            block_word_off: &mut alias_off,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert_eq!(alias_out.stats.movs_eliminated, 0);
        assert_eq!(
            alias_opt.iter().filter(|&&w| w == enc::ldr(Size::X, X17, SP, 2)).count(),
            2,
            "aliasing non-SP store `str x1, [x2, #0]` must preserve second `ldr x17, [sp, #0x10]`"
        );

        // 3. Partial overlapping same-base store: `ldr x17, [sp, #0x10]; str w1, [sp, #0x14]; ldr x17, [sp, #0x10]`
        //    -> `[0x14, 0x18)` overlaps `[0x10, 0x18)` -> second `ldr` MUST be preserved!
        let overlap_raw = [
            enc::ldr(Size::X, X17, SP, 2), // [sp, #0x10..#0x18)
            enc::str(Size::W, X1, SP, 5),  // [sp, #0x14..#0x18) — overlaps upper 4 bytes!
            enc::ldr(Size::X, X17, SP, 2), // MUST be preserved!
            enc::add(X0, X17, X1),
            enc::ret(),
        ];
        let mut overlap_opt = words_vec(&overlap_raw, alloc);
        let mut overlap_off = [0usize];
        let overlap_out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut overlap_opt,
            block_word_off: &mut overlap_off,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert_eq!(overlap_out.stats.movs_eliminated, 0);
        assert_eq!(
            overlap_opt.iter().filter(|&&w| w == enc::ldr(Size::X, X17, SP, 2)).count(),
            2,
            "overlapping same-base store must preserve second `ldr x17, [sp, #0x10]`"
        );

        // 4. Sign-extending `ldrsw x9, [sp, #0x10]` MUST NOT be confused with unsigned `ldr w9, [sp, #0x10]`!
        let ldrsw_x9_sp_16 = 0xb980_0000 | (4u32 << 10) | (SP << 5) | X9;
        assert_eq!(decode_inst(ldrsw_x9_sp_16).kind, DecodedKind::Other);
        let sext_raw = [
            enc::ldr(Size::W, X9, SP, 4), // unsigned zero-extending load of [sp, #0x10]
            ldrsw_x9_sp_16,               // signed sign-extending load of [sp, #0x10] -> MUST stay!
            enc::add(X0, X9, X1),
            enc::ret(),
        ];
        let mut sext_opt = words_vec(&sext_raw, alloc);
        let mut sext_off = [0usize];
        let sext_out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut sext_opt,
            block_word_off: &mut sext_off,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert_eq!(sext_out.stats.movs_eliminated, 0);
        assert!(sext_opt.iter().any(|&w| w == ldrsw_x9_sp_16), "`ldrsw` must be preserved");

        // 5. Base-clobbering load `ldr x17, [x17, #0]; ldr x17, [x17, #0]` (`rt == rn`) -> MUST keep both!
        let chase_raw = [
            enc::ldr(Size::X, X17, X17, 0),
            enc::ldr(Size::X, X17, X17, 0),
            enc::add(X0, X17, X1),
            enc::ret(),
        ];
        let mut chase_opt = words_vec(&chase_raw, alloc);
        let mut chase_off = [0usize];
        let chase_out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut chase_opt,
            block_word_off: &mut chase_off,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert_eq!(chase_out.stats.movs_eliminated, 0);
        assert_eq!(
            chase_opt.iter().filter(|&&w| w == enc::ldr(Size::X, X17, X17, 0)).count(),
            2,
            "pointer-chasing `ldr x17, [x17]` (`rt == rn`) must keep both loads"
        );

        // 6. Target boundary between two identical `ldr x17, [sp, #0x10]` clears facts!
        let bnd_raw = [
            // Block 0:
            enc::ldr(Size::X, X17, SP, 2),
            // Block 1 (target boundary):
            enc::ldr(Size::X, X17, SP, 2),
            enc::add(X0, X17, X1),
            enc::ret(),
        ];
        let mut bnd_opt = words_vec(&bnd_raw, alloc);
        let mut bnd_off = [0usize, 1];
        let bnd_out = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut bnd_opt,
            block_word_off: &mut bnd_off,
            block_order: &[0, 1],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert_eq!(bnd_out.stats.movs_eliminated, 0);
        assert_eq!(
            bnd_opt.iter().filter(|&&w| w == enc::ldr(Size::X, X17, SP, 2)).count(),
            2,
            "target boundary must clear active load facts"
        );

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let ex_xzr_orig = exec_leaf(&xzr_raw, alloc);
            let ex_xzr_opt = exec_leaf(&xzr_opt, alloc);
            // SAFETY: Valid AAPCS64 leaf function using 32B stack frame and caller-saved regs.
            let f_xzr_orig: extern "C" fn(u64, u64) -> u64 = unsafe { ex_xzr_orig.as_fn() };
            // SAFETY: Same verified signature.
            let f_xzr_opt: extern "C" fn(u64, u64) -> u64 = unsafe { ex_xzr_opt.as_fn() };
            assert_eq!(f_xzr_opt(0, 42), f_xzr_orig(0, 42));
            assert_eq!(f_xzr_opt(0, 42), 42);

            let ex_alias_orig = exec_leaf(&alias_raw, alloc);
            let ex_alias_opt = exec_leaf(&alias_opt, alloc);
            // SAFETY: Valid AAPCS64 leaf function using 32B stack frame and caller-saved regs.
            let f_alias_orig: extern "C" fn(u64, u64) -> u64 = unsafe { ex_alias_orig.as_fn() };
            // SAFETY: Same verified signature.
            let f_alias_opt: extern "C" fn(u64, u64) -> u64 = unsafe { ex_alias_opt.as_fn() };
            assert_eq!(f_alias_opt(111, 222), f_alias_orig(111, 222));
            assert_eq!(f_alias_opt(111, 222), 222);
        }
    }

    #[test]
    fn peephole_sink_sub_into_cmp_requires_distinct_rd_rn() {
        let alloc = test_alloc();

        // Negative control (`rd == rn`): `sub x0, x0, #8; cmp x0, #8; cset x0, hs; ret`.
        // For x0 = 8, post-sub x0 = 0 < 8 so `cset x0, hs` MUST return 0 (NOT 1!).
        let self_clobber_raw = [
            enc::sub_imm(X0, X0, 8, false),
            enc::cmp_imm(X0, 8),
            enc::cset(X0, Cond::Hs),
            enc::ret(),
        ];
        let mut self_opt = words_vec(&self_clobber_raw, alloc);
        let mut off = [0usize];
        let _ = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut self_opt,
            block_word_off: &mut off,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert!(
            self_opt.iter().any(|&w| w == enc::cmp_imm(X0, 8)),
            "`sub x0, x0, #8; cmp x0, #8` (rd == rn) must NOT fuse into `subs x0, x0, #8`"
        );

        // Positive case (`rd != rn`): `sub x1, x0, #8; cmp x0, #8; csel x0, x1, x0, hs; ret`.
        let distinct_raw = [
            enc::sub_imm(X1, X0, 8, false),
            enc::cmp_imm(X0, 8),
            enc::csel(X0, X1, X0, Cond::Hs),
            enc::ret(),
        ];
        let mut dist_opt = words_vec(&distinct_raw, alloc);
        let mut dist_off = [0usize];
        let _ = optimize_mach_a64(&mut MachOptimizeInput {
            words: &mut dist_opt,
            block_word_off: &mut dist_off,
            block_order: &[0],
            patch_word_idxs: &mut [],
            patch_target_blocks: &[],
            reloc_byte_offs: &mut [],
            trap_byte_offs: &mut [],
            cold_stub_word_idxs: &[],
            extra_word_idxs: &mut [],
            align16_blocks: &[],
            volatile_word_idxs: &[],
            ret_live_gprs: RET_LIVE_GPRS,
            alloc,
        });
        assert!(
            dist_opt.iter().any(|&w| w == enc::subs_imm(X1, X0, 8, false)),
            "`sub x1, x0, #8; cmp x0, #8` (rd != rn) should fuse into `subs x1, x0, #8`"
        );

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let ex_orig = exec_leaf(&self_clobber_raw, alloc);
            let ex_opt = exec_leaf(&self_opt, alloc);
            // SAFETY: Valid AAPCS64 leaf function using only caller-saved X0.
            let f_orig: extern "C" fn(u64) -> u64 = unsafe { ex_orig.as_fn() };
            // SAFETY: Same verified signature.
            let f_opt: extern "C" fn(u64) -> u64 = unsafe { ex_opt.as_fn() };
            for &x in &[0u64, 7, 8, 15, 16, 24] {
                assert_eq!(f_opt(x), f_orig(x), "mismatch at x={x}");
            }
            assert_eq!(f_opt(8), 0);
            assert_eq!(f_opt(16), 1);

            let ex_d_orig = exec_leaf(&distinct_raw, alloc);
            let ex_d_opt = exec_leaf(&dist_opt, alloc);
            // SAFETY: Valid AAPCS64 leaf function using only caller-saved X0, X1.
            let fd_orig: extern "C" fn(u64) -> u64 = unsafe { ex_d_orig.as_fn() };
            // SAFETY: Same verified signature.
            let fd_opt: extern "C" fn(u64) -> u64 = unsafe { ex_d_opt.as_fn() };
            for &x in &[0u64, 7, 8, 15, 16, 24] {
                assert_eq!(fd_opt(x), fd_orig(x), "distinct mismatch at x={x}");
            }
        }
    }
}
