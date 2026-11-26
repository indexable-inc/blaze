#![cfg_attr(not(test), forbid(unsafe_code))]
//! Typed SoA SSA AArch64 Machine IR (`MFunc`), Structural Verifier (`mverify`), and
//! 1-Pass Binary Encoder (`mencode`) for `TierName::Sota` (V3 Phase 2).
//!
//! Replaces `u32`-word disassembly (`mach_a64.rs`) with a strongly typed, safety-by-construction
//! Structure-of-Arrays (SoA) machine IR backed by the caller's explicit [`Alloc`]:
//! - Separate [`PReg::Gpr`], [`PReg::Sp`], [`PReg::Xzr`], [`PReg::V`], and [`PReg::Nzcv`]
//!   variants make ARM64 register-31 `SP` vs `XZR` confusion impossible at the type level.
//! - Explicit virtual flags (`vflag_def`, `vflag_use`) enforce the architectural single-live-NZCV
//!   invariant both pre-RA (SSA) and post-RA.
//! - Attached instruction metadata ([`MNote`]) flows directly into [`mencode`], emitting exact
//!   relocation, trap, unwind, and jump-table offsets in one pass with zero manual remapping and
//!   automatic `Imm14` (`±32 KiB`) / `Imm19` (`±1 MiB`) branch range relaxation.
//! - All binary encodings delegate directly to [`crate::enc`] (Rule 30: one encoding home).

use core::fmt;
use core::ops::Range;

use blaze_own::{Alloc, Vec};
use blaze_tier::{Os, RelocKind};

use crate::enc::{self, Cond, Ftype, Size};

/// Sentinel `vflag_def` / `vflag_use` value meaning "no virtual flag".
pub const NO_VFLAG: u32 = u32::MAX;

/// Sentinel `note_idx` / `MNote::next` value meaning "no attached note".
pub const NO_NOTE: u32 = u32::MAX;

/// Maximum branch relaxation rounds (bounded progress, Rule 18).
const MAX_RELAX_ROUNDS: usize = 16;

/// `Imm14` (`tbz`/`tbnz`) signed word displacement range (`[-8192, +8191]`, ±32 KiB).
const IMM14_MIN_WORDS: i64 = -(1 << 13);
const IMM14_MAX_WORDS: i64 = (1 << 13) - 1;

/// `Imm19` (`b.cc`/`cbz`/`cbnz`) signed word displacement range (`[-262_144, +262_143]`, ±1 MiB).
const IMM19_MIN_WORDS: i64 = -(1 << 18);
const IMM19_MAX_WORDS: i64 = (1 << 18) - 1;

/// `Imm26` (`b`/`bl`) signed word displacement range (`[-33_554_432, +33_554_431]`, ±128 MiB).
const IMM26_MIN_WORDS: i64 = -(1 << 25);
const IMM26_MAX_WORDS: i64 = (1 << 25) - 1;

/// Conservative default GPR bitmask live at `ret`: `X0..=X7`, `X19..=X30`, and `SP`.
pub const DEFAULT_RET_LIVE_GPRS: u32 = 0xfff8_00ff;

// -----------------------------------------------------------------------------
// Zero-`as` numeric conversion helpers
// -----------------------------------------------------------------------------

#[inline]
const fn u8_to_u32(v: u8) -> u32 {
    u32::from_le_bytes([v, 0, 0, 0])
}

#[inline]
const fn u16_to_u32(v: u16) -> u32 {
    let b = v.to_le_bytes();
    u32::from_le_bytes([b[0], b[1], 0, 0])
}

#[inline]
const fn trunc_u32(v: u64) -> u32 {
    let b = v.to_le_bytes();
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

#[inline]
const fn chunk16(v: u64, hw: u32) -> u32 {
    let shifted = v >> (hw * 16);
    let b = shifted.to_le_bytes();
    u32::from_le_bytes([b[0], b[1], 0, 0])
}

#[inline]
fn to_idx(v: u32) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

#[inline]
fn u32_of(v: usize) -> Option<u32> {
    u32::try_from(v).ok()
}

// -----------------------------------------------------------------------------
// Core typed identifiers & operands (Rules 73, 76, 85)
// -----------------------------------------------------------------------------

/// SSA virtual register identifier (index into [`MFunc::vreg_class`]).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct VReg(pub u32);

/// Register bank and width class of a [`VReg`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum VRegClass {
    /// 32-bit general-purpose integer (`W` view).
    Gpr32,
    /// 64-bit general-purpose integer or pointer (`X` view).
    Gpr64,
    /// 32-bit IEEE-754 scalar float (`S` view).
    Fp32,
    /// 64-bit IEEE-754 scalar float (`D` view).
    Fp64,
    /// 128-bit NEON SIMD vector (`Q`/`V` view).
    Vec128,
    /// Explicit virtual NZCV condition flags.
    Nzcv,
}

impl VRegClass {
    #[inline]
    pub const fn is_gpr(self) -> bool {
        matches!(self, Self::Gpr32 | Self::Gpr64)
    }

    #[inline]
    pub const fn is_fp_or_vec(self) -> bool {
        matches!(self, Self::Fp32 | Self::Fp64 | Self::Vec128)
    }

    #[inline]
    pub const fn is_flags(self) -> bool {
        matches!(self, Self::Nzcv)
    }
}

/// Physical AArch64 register with type-level separation of `SP` vs `XZR` (both hardware reg 31).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum PReg {
    /// General-purpose register `X0..=X30` (`0..=30`; never 31).
    Gpr(u8),
    /// Stack pointer (`SP`). Only legal in `SP`-accepting operand positions.
    Sp,
    /// Zero register (`XZR` / `WZR`). Only legal in `ZR`-accepting operand positions.
    Xzr,
    /// SIMD / FP register `V0..=V31` (`0..=31`).
    V(u8),
    /// Physical `NZCV` condition flags register.
    Nzcv,
}

impl PReg {
    pub const X0: Self = Self::Gpr(0);
    pub const X1: Self = Self::Gpr(1);
    pub const X2: Self = Self::Gpr(2);
    pub const X3: Self = Self::Gpr(3);
    pub const X4: Self = Self::Gpr(4);
    pub const X5: Self = Self::Gpr(5);
    pub const X6: Self = Self::Gpr(6);
    pub const X7: Self = Self::Gpr(7);
    pub const X8: Self = Self::Gpr(8);
    pub const X9: Self = Self::Gpr(9);
    pub const X10: Self = Self::Gpr(10);
    pub const X11: Self = Self::Gpr(11);
    pub const X12: Self = Self::Gpr(12);
    pub const X13: Self = Self::Gpr(13);
    pub const X14: Self = Self::Gpr(14);
    pub const X15: Self = Self::Gpr(15);
    pub const X16: Self = Self::Gpr(16);
    pub const X17: Self = Self::Gpr(17);
    pub const X18: Self = Self::Gpr(18);
    pub const X19: Self = Self::Gpr(19);
    pub const X20: Self = Self::Gpr(20);
    pub const X21: Self = Self::Gpr(21);
    pub const X22: Self = Self::Gpr(22);
    pub const X23: Self = Self::Gpr(23);
    pub const X24: Self = Self::Gpr(24);
    pub const X25: Self = Self::Gpr(25);
    pub const X26: Self = Self::Gpr(26);
    pub const X27: Self = Self::Gpr(27);
    pub const X28: Self = Self::Gpr(28);
    pub const FP: Self = Self::Gpr(29);
    pub const LR: Self = Self::Gpr(30);

    /// Hardware 5-bit register number for a GPR-or-`XZR` operand (`Sp` is rejected).
    #[inline]
    pub const fn gpr_or_zr_enc(self) -> Option<u32> {
        match self {
            Self::Gpr(r) if r <= 30 => Some(u8_to_u32(r)),
            Self::Xzr => Some(enc::ZR),
            Self::Gpr(_) | Self::Sp | Self::V(_) | Self::Nzcv => None,
        }
    }

    /// Hardware 5-bit register number for a GPR-or-`SP` operand (`Xzr` is rejected).
    #[inline]
    pub const fn gpr_or_sp_enc(self) -> Option<u32> {
        match self {
            Self::Gpr(r) if r <= 30 => Some(u8_to_u32(r)),
            Self::Sp => Some(enc::SP),
            Self::Gpr(_) | Self::Xzr | Self::V(_) | Self::Nzcv => None,
        }
    }

    /// Hardware 5-bit register number for a SIMD/FP `V0..=V31` operand.
    #[inline]
    pub const fn vec_enc(self) -> Option<u32> {
        match self {
            Self::V(r) if r <= 31 => Some(u8_to_u32(r)),
            Self::V(_) | Self::Gpr(_) | Self::Sp | Self::Xzr | Self::Nzcv => None,
        }
    }

    /// Whether `self` is an architecturally valid register encoding (independent of OS).
    #[inline]
    pub const fn is_arch_valid(self) -> bool {
        match self {
            Self::Gpr(r) => r <= 30,
            Self::V(r) => r <= 31,
            Self::Sp | Self::Xzr | Self::Nzcv => true,
        }
    }

    /// Whether `self` is reserved by the target OS platform ABI (`X18` on macOS).
    #[inline]
    pub const fn is_os_reserved(self, os: Os) -> bool {
        matches!((self, os), (Self::Gpr(18), Os::MacOs))
    }
}

/// Machine register operand: either an SSA virtual register or a physical AArch64 register.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum MReg {
    V(VReg),
    P(PReg),
}

impl MReg {
    pub const SP: Self = Self::P(PReg::Sp);
    pub const XZR: Self = Self::P(PReg::Xzr);
    pub const NZCV: Self = Self::P(PReg::Nzcv);

    #[inline]
    pub const fn v(id: u32) -> Self {
        Self::V(VReg(id))
    }

    #[inline]
    pub const fn p(reg: PReg) -> Self {
        Self::P(reg)
    }

    #[inline]
    pub const fn gpr(r: u8) -> Self {
        Self::P(PReg::Gpr(r))
    }

    #[inline]
    pub const fn vec(r: u8) -> Self {
        Self::P(PReg::V(r))
    }

    #[inline]
    pub const fn is_virtual(self) -> bool {
        matches!(self, Self::V(_))
    }

    #[inline]
    pub const fn as_phys(self) -> Option<PReg> {
        match self {
            Self::P(p) => Some(p),
            Self::V(_) => None,
        }
    }

    #[inline]
    pub const fn as_virt(self) -> Option<VReg> {
        match self {
            Self::V(v) => Some(v),
            Self::P(_) => None,
        }
    }
}

/// Basic block index in [`MFunc::block_insts`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct MBlock(pub u32);

/// Instruction index in [`MFunc::ops`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct MInst(pub u32);

/// Shifted-register ALU shift kind and immediate amount.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum MShift {
    Lsl(u8),
    Lsr(u8),
    Asr(u8),
}

impl MShift {
    #[inline]
    pub const fn kind_bits(self) -> u32 {
        match self {
            Self::Lsl(_) => 0,
            Self::Lsr(_) => 1,
            Self::Asr(_) => 2,
        }
    }

    #[inline]
    pub const fn amount(self) -> u8 {
        match self {
            Self::Lsl(n) | Self::Lsr(n) | Self::Asr(n) => n,
        }
    }
}

/// Extended-register operand extension for `AddExt` / `SubExt`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum MExt {
    Uxtb,
    Uxth,
    Uxtw,
    Sxtw,
}

/// Memory access width and register file classification.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum MemSize {
    B8,
    H16,
    W32,
    X64,
    S32,
    D64,
    Q128,
}

impl MemSize {
    #[inline]
    pub const fn is_fp_or_vec(self) -> bool {
        matches!(self, Self::S32 | Self::D64 | Self::Q128)
    }

    #[inline]
    pub const fn byte_size(self) -> u32 {
        match self {
            Self::B8 => 1,
            Self::H16 => 2,
            Self::W32 | Self::S32 => 4,
            Self::X64 | Self::D64 => 8,
            Self::Q128 => 16,
        }
    }

    #[inline]
    pub const fn scale_shift(self) -> u8 {
        match self {
            Self::B8 => 0,
            Self::H16 => 1,
            Self::W32 | Self::S32 => 2,
            Self::X64 | Self::D64 => 3,
            Self::Q128 => 4,
        }
    }

    #[inline]
    pub(crate) const fn to_enc_size(self) -> Option<Size> {
        match self {
            Self::B8 => Some(Size::B),
            Self::H16 => Some(Size::H),
            Self::W32 | Self::S32 => Some(Size::W),
            Self::X64 | Self::D64 => Some(Size::X),
            Self::Q128 => None,
        }
    }
}

/// Memory addressing mode for `Ldr` and `Str`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum AddrMode {
    /// `[base, #off]` (scaled unsigned `imm12` or unscaled signed `simm9` in `-256..=255`).
    BaseOff { base: MReg, off: i32 },
    /// `[base, idx{, lsl #shift}]` (`shift` is `0` or `size.scale_shift()`).
    ScaledIdx { base: MReg, idx: MReg, shift: u8 },
    /// `[base], #off` (post-index writeback, `off` in `-256..=255`).
    PostIdx { base: MReg, off: i16 },
    /// `[base, #off]!` (pre-index writeback, `off` in `-256..=255`).
    PreIdx { base: MReg, off: i16 },
}

/// AArch64 condition code (typed wrapper over [`crate::enc::Cond`]).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u8)]
pub enum MCond {
    Eq = 0,
    Ne = 1,
    Hs = 2,
    Lo = 3,
    Mi = 4,
    Pl = 5,
    Vs = 6,
    Vc = 7,
    Hi = 8,
    Ls = 9,
    Ge = 10,
    Lt = 11,
    Gt = 12,
    Le = 13,
    Al = 14,
    Nv = 15,
}

impl MCond {
    /// Logically opposite condition code (flips bit 0; note `Al`/`Nv` are both unconditional).
    #[inline]
    pub const fn invert(self) -> Self {
        match self {
            Self::Eq => Self::Ne,
            Self::Ne => Self::Eq,
            Self::Hs => Self::Lo,
            Self::Lo => Self::Hs,
            Self::Mi => Self::Pl,
            Self::Pl => Self::Mi,
            Self::Vs => Self::Vc,
            Self::Vc => Self::Vs,
            Self::Hi => Self::Ls,
            Self::Ls => Self::Hi,
            Self::Ge => Self::Lt,
            Self::Lt => Self::Ge,
            Self::Gt => Self::Le,
            Self::Le => Self::Gt,
            Self::Al => Self::Nv,
            Self::Nv => Self::Al,
        }
    }

    #[inline]
    pub(crate) const fn to_enc(self) -> Cond {
        match self {
            Self::Eq => Cond::Eq,
            Self::Ne => Cond::Ne,
            Self::Hs => Cond::Hs,
            Self::Lo => Cond::Lo,
            Self::Mi => Cond::Mi,
            Self::Pl => Cond::Pl,
            Self::Vs => Cond::Vs,
            Self::Vc => Cond::Vc,
            Self::Hi => Cond::Hi,
            Self::Ls => Cond::Ls,
            Self::Ge => Cond::Ge,
            Self::Lt => Cond::Lt,
            Self::Gt => Cond::Gt,
            Self::Le => Cond::Le,
            Self::Al => Cond::Al,
            Self::Nv => Cond::Nv,
        }
    }

    #[inline]
    pub(crate) const fn from_enc(c: Cond) -> Self {
        match c {
            Cond::Eq => Self::Eq,
            Cond::Ne => Self::Ne,
            Cond::Hs => Self::Hs,
            Cond::Lo => Self::Lo,
            Cond::Mi => Self::Mi,
            Cond::Pl => Self::Pl,
            Cond::Vs => Self::Vs,
            Cond::Vc => Self::Vc,
            Cond::Hi => Self::Hi,
            Cond::Ls => Self::Ls,
            Cond::Ge => Self::Ge,
            Cond::Lt => Self::Lt,
            Cond::Gt => Self::Gt,
            Cond::Le => Self::Le,
            Cond::Al => Self::Al,
            Cond::Nv => Self::Nv,
        }
    }
}

/// Bitwise logical operation family.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum LogicalKind {
    And,
    Orr,
    Eor,
    Ands,
}

/// Variable-shift operation family.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ShiftKind {
    Lsl,
    Lsr,
    Asr,
    Ror,
}

/// Scalar floating-point operation selector for [`MOp::FpAlu`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum FpAluOp {
    Add,
    Sub,
    Mul,
    Div,
    Madd,
    Msub,
    Mov,
    Neg,
    Abs,
    Sqrt,
    RintN,
    RintP,
    RintM,
    RintZ,
    RintA,
    /// `fcvt dd, sn` (`f32` -> `f64`).
    CvtS2D,
    /// `fcvt sd, dn` (`f64` -> `f32`).
    CvtD2S,
    /// `fmov dd/sd, xn/wn` (bitcast GPR -> FP).
    MovFromGpr,
    /// `fmov xd/wd, dn/sn` (bitcast FP -> GPR).
    MovToGpr,
    /// `scvtf dd/sd, xn/wn`.
    Scvtf { int_wide: bool },
    /// `ucvtf dd/sd, xn/wn`.
    Ucvtf { int_wide: bool },
    /// `fcvtzs xd/wd, dn/sn`.
    Fcvtzs { int_wide: bool },
    /// `fcvtzu xd/wd, dn/sn`.
    Fcvtzu { int_wide: bool },
}

/// 128-bit NEON SIMD operation selector for [`MOp::VecAlu`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum VecAluOp {
    Zero,
    Mov,
    Not,
    And,
    Orr,
    Eor,
    /// `bsl vd.16b, vn.16b, vm.16b` (`vd` is also read).
    Bsl,
    /// `bit vd.16b, vn.16b, vm.16b` (`vd` is also read).
    Bit,
    /// `bif vd.16b, vn.16b, vm.16b` (`vd` is also read).
    Bif,
    Add,
    Sub,
    Mul,
    /// `mla vd.T, vn.T, vm.T` (`vd` is also read).
    Mla,
    /// `mls vd.T, vn.T, vm.T` (`vd` is also read).
    Mls,
    Smin,
    Smax,
    Umin,
    Umax,
    Sqadd,
    Uqadd,
    Sqsub,
    Uqsub,
    Fadd,
    Fsub,
    Fmul,
    /// `fmla vd.T, vn.T, vm.T` (`vd` is also read).
    Fmla,
    /// `fmls vd.T, vn.T, vm.T` (`vd` is also read).
    Fmls,
    Cmeq,
    Cmgt,
    Cmge,
    Cmhi,
    Cmhs,
    /// `dup vd.T, rn` (`rn` is a GPR).
    DupGen,
    /// `dup vd.T, vn.Ts[0]`.
    DupElem0,
    /// `addv b/h/s, vn.T`.
    Addv,
    /// `addp dd, vn.2d`.
    AddpD,
    /// `faddp sd/dd, vn.2s/2d`.
    FaddpScalar,
    /// `faddp vd.4s/2d, vn.4s/2d, vm.4s/2d`.
    FaddpV,
    /// `uminv bd, vn.16b`.
    UminvB,
    /// `umov wd/xd, vn.Ts[lane]` (`dst` is a GPR).
    UmovLane { lane: u8 },
    /// `ext vd.16b, vn.16b, vm.16b, #imm4`.
    Ext { imm4: u8 },
}

/// Typed AArch64 machine instruction enum.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum MOp {
    Mov { dst: MReg, src: MReg, wide: bool },
    MovImm { dst: MReg, imm: u64, wide: bool },
    MovSp { dst: MReg, src: MReg },
    AddImm { dst: MReg, rn: MReg, imm12: u16, shift12: bool, wide: bool, sets_flags: bool },
    SubImm { dst: MReg, rn: MReg, imm12: u16, shift12: bool, wide: bool, sets_flags: bool },
    AddReg { dst: MReg, rn: MReg, rm: MReg, shift: Option<MShift>, wide: bool, sets_flags: bool },
    SubReg { dst: MReg, rn: MReg, rm: MReg, shift: Option<MShift>, wide: bool, sets_flags: bool },
    AddExt { dst: MReg, rn: MReg, rm: MReg, ext: MExt, wide: bool, sets_flags: bool },
    SubExt { dst: MReg, rn: MReg, rm: MReg, ext: MExt, wide: bool, sets_flags: bool },
    LogicalImm { dst: MReg, rn: MReg, imm: u64, kind: LogicalKind, wide: bool },
    LogicalReg { dst: MReg, rn: MReg, rm: MReg, shift: Option<MShift>, invert_rm: bool, kind: LogicalKind, wide: bool },
    Mvn { dst: MReg, rm: MReg, shift: Option<MShift>, wide: bool },
    ShiftVar { dst: MReg, rn: MReg, rm: MReg, kind: ShiftKind, wide: bool },
    Ubfm { dst: MReg, rn: MReg, immr: u8, imms: u8, wide: bool },
    Sbfm { dst: MReg, rn: MReg, immr: u8, imms: u8, wide: bool },
    Bfxil { dst: MReg, rn: MReg, lsb: u8, width: u8, wide: bool },
    Extr { dst: MReg, rn: MReg, rm: MReg, lsb: u8, wide: bool },
    Rbit { dst: MReg, rn: MReg, wide: bool },
    Clz { dst: MReg, rn: MReg, wide: bool },
    Rev { dst: MReg, rn: MReg, wide: bool },
    Mul { dst: MReg, rn: MReg, rm: MReg, wide: bool },
    Madd { dst: MReg, rn: MReg, rm: MReg, ra: MReg, wide: bool },
    Msub { dst: MReg, rn: MReg, rm: MReg, ra: MReg, wide: bool },
    Umulh { dst: MReg, rn: MReg, rm: MReg },
    Smulh { dst: MReg, rn: MReg, rm: MReg },
    /// 64-bit add/sub with carry (`adc` / `adcs` / `sbc` / `sbcs`) for multi-word `i128` ops.
    CarryAddSub { dst: MReg, rn: MReg, rm: MReg, is_sub: bool, sets_flags: bool },
    Udiv { dst: MReg, rn: MReg, rm: MReg, wide: bool },
    Sdiv { dst: MReg, rn: MReg, rm: MReg, wide: bool },
    Umin { dst: MReg, rn: MReg, rm: MReg, wide: bool },
    Umax { dst: MReg, rn: MReg, rm: MReg, wide: bool },
    Smin { dst: MReg, rn: MReg, rm: MReg, wide: bool },
    Smax { dst: MReg, rn: MReg, rm: MReg, wide: bool },
    CmpImm { rn: MReg, imm12: u16, shift12: bool, wide: bool },
    CmpReg { rn: MReg, rm: MReg, shift: Option<MShift>, wide: bool },
    TstImm { rn: MReg, imm: u64, wide: bool },
    TstReg { rn: MReg, rm: MReg, wide: bool },
    CcmpImm { rn: MReg, imm5: u8, nzcv: u8, cond: MCond, wide: bool },
    CcmpReg { rn: MReg, rm: MReg, nzcv: u8, cond: MCond, wide: bool },
    Csel { dst: MReg, rn: MReg, rm: MReg, cond: MCond, wide: bool },
    Csinc { dst: MReg, rn: MReg, rm: MReg, cond: MCond, wide: bool },
    Csinv { dst: MReg, rn: MReg, rm: MReg, cond: MCond, wide: bool },
    Csneg { dst: MReg, rn: MReg, rm: MReg, cond: MCond, wide: bool },
    Ldr { dst: MReg, addr: AddrMode, size: MemSize, signed: bool },
    Str { src: MReg, addr: AddrMode, size: MemSize },
    Ldp { dst0: MReg, dst1: MReg, base: MReg, off: i16, size: MemSize, post_idx: bool, pre_idx: bool },
    Stp { src0: MReg, src1: MReg, base: MReg, off: i16, size: MemSize, post_idx: bool, pre_idx: bool },
    FpAlu { op: FpAluOp, dst: MReg, rn: MReg, rm: Option<MReg>, ra: Option<MReg>, double: bool },
    FpCmp { rn: MReg, rm: MReg, double: bool },
    Fcsel { dst: MReg, rn: MReg, rm: MReg, cond: MCond, double: bool },
    VecAlu { op: VecAluOp, dst: MReg, rn: MReg, rm: Option<MReg>, elem_size: u8 },
    Adrp { dst: MReg },
    AddLo12 { dst: MReg, rn: MReg },
    AdrTable { dst: MReg, table_id: u32 },
    Bl,
    Blr { target: MReg },
    Br { target: MReg },
    B { target: MBlock },
    BCond { cond: MCond, target: MBlock },
    Cbz { rt: MReg, target: MBlock, wide: bool },
    Cbnz { rt: MReg, target: MBlock, wide: bool },
    Tbz { rt: MReg, bit: u8, target: MBlock },
    Tbnz { rt: MReg, bit: u8, target: MBlock },
    Ret,
    Brk { imm: u16 },
    Prfm { base: MReg, imm12: u16 },
    Udf,
    Nop,
    /// SSA-only parallel register copy referencing `MFunc::copy_pairs[start..start + len]`.
    ParallelCopy { start: u32, len: u16 },
    /// Escape hatch for rare LSE/SME2/CRC32/sysreg words with explicit register/flag effects.
    RawWord {
        word: u32,
        defs_gpr: u32,
        uses_gpr: u32,
        defs_fp: u32,
        uses_fp: u32,
        writes_flags: bool,
        reads_flags: bool,
        is_mem_barrier: bool,
    },
}

impl MOp {
    /// True if this instruction writes the architectural `NZCV` flags (including calls).
    #[inline]
    pub const fn writes_flags(&self) -> bool {
        match *self {
            Self::AddImm { sets_flags, .. }
            | Self::SubImm { sets_flags, .. }
            | Self::AddReg { sets_flags, .. }
            | Self::SubReg { sets_flags, .. }
            | Self::AddExt { sets_flags, .. }
            | Self::SubExt { sets_flags, .. }
            | Self::CarryAddSub { sets_flags, .. } => sets_flags,
            Self::LogicalImm { kind, .. } | Self::LogicalReg { kind, .. } => {
                matches!(kind, LogicalKind::Ands)
            }
            Self::CmpImm { .. }
            | Self::CmpReg { .. }
            | Self::TstImm { .. }
            | Self::TstReg { .. }
            | Self::CcmpImm { .. }
            | Self::CcmpReg { .. }
            | Self::FpCmp { .. }
            | Self::Bl
            | Self::Blr { .. } => true,
            Self::RawWord { writes_flags, .. } => writes_flags,
            Self::Mov { .. }
            | Self::MovImm { .. }
            | Self::MovSp { .. }
            | Self::Mvn { .. }
            | Self::ShiftVar { .. }
            | Self::Ubfm { .. }
            | Self::Sbfm { .. }
            | Self::Bfxil { .. }
            | Self::Extr { .. }
            | Self::Rbit { .. }
            | Self::Clz { .. }
            | Self::Rev { .. }
            | Self::Mul { .. }
            | Self::Madd { .. }
            | Self::Msub { .. }
            | Self::Umulh { .. }
            | Self::Smulh { .. }
            | Self::Udiv { .. }
            | Self::Sdiv { .. }
            | Self::Umin { .. }
            | Self::Umax { .. }
            | Self::Smin { .. }
            | Self::Smax { .. }
            | Self::Csel { .. }
            | Self::Csinc { .. }
            | Self::Csinv { .. }
            | Self::Csneg { .. }
            | Self::Ldr { .. }
            | Self::Str { .. }
            | Self::Ldp { .. }
            | Self::Stp { .. }
            | Self::FpAlu { .. }
            | Self::Fcsel { .. }
            | Self::VecAlu { .. }
            | Self::Adrp { .. }
            | Self::AddLo12 { .. }
            | Self::AdrTable { .. }
            | Self::Br { .. }
            | Self::B { .. }
            | Self::BCond { .. }
            | Self::Cbz { .. }
            | Self::Cbnz { .. }
            | Self::Tbz { .. }
            | Self::Tbnz { .. }
            | Self::Ret
            | Self::Brk { .. }
            | Self::Prfm { .. }
            | Self::Udf
            | Self::Nop
            | Self::ParallelCopy { .. } => false,
        }
    }

    /// True if this instruction reads the architectural `NZCV` flags.
    #[inline]
    pub const fn reads_flags(&self) -> bool {
        match *self {
            Self::CcmpImm { .. }
            | Self::CcmpReg { .. }
            | Self::CarryAddSub { .. }
            | Self::Csel { .. }
            | Self::Csinc { .. }
            | Self::Csinv { .. }
            | Self::Csneg { .. }
            | Self::Fcsel { .. } => true,
            Self::BCond { cond, .. } => !matches!(cond, MCond::Al | MCond::Nv),
            Self::RawWord { reads_flags, .. } => reads_flags,
            Self::Mov { .. }
            | Self::MovImm { .. }
            | Self::MovSp { .. }
            | Self::AddImm { .. }
            | Self::SubImm { .. }
            | Self::AddReg { .. }
            | Self::SubReg { .. }
            | Self::AddExt { .. }
            | Self::SubExt { .. }
            | Self::LogicalImm { .. }
            | Self::LogicalReg { .. }
            | Self::Mvn { .. }
            | Self::ShiftVar { .. }
            | Self::Ubfm { .. }
            | Self::Sbfm { .. }
            | Self::Bfxil { .. }
            | Self::Extr { .. }
            | Self::Rbit { .. }
            | Self::Clz { .. }
            | Self::Rev { .. }
            | Self::Mul { .. }
            | Self::Madd { .. }
            | Self::Msub { .. }
            | Self::Umulh { .. }
            | Self::Smulh { .. }
            | Self::Udiv { .. }
            | Self::Sdiv { .. }
            | Self::Umin { .. }
            | Self::Umax { .. }
            | Self::Smin { .. }
            | Self::Smax { .. }
            | Self::CmpImm { .. }
            | Self::CmpReg { .. }
            | Self::TstImm { .. }
            | Self::TstReg { .. }
            | Self::Ldr { .. }
            | Self::Str { .. }
            | Self::Ldp { .. }
            | Self::Stp { .. }
            | Self::FpAlu { .. }
            | Self::FpCmp { .. }
            | Self::VecAlu { .. }
            | Self::Adrp { .. }
            | Self::AddLo12 { .. }
            | Self::AdrTable { .. }
            | Self::Bl
            | Self::Blr { .. }
            | Self::Br { .. }
            | Self::B { .. }
            | Self::Cbz { .. }
            | Self::Cbnz { .. }
            | Self::Tbz { .. }
            | Self::Tbnz { .. }
            | Self::Ret
            | Self::Brk { .. }
            | Self::Prfm { .. }
            | Self::Udf
            | Self::Nop
            | Self::ParallelCopy { .. } => false,
        }
    }

    /// True if this instruction never falls through to the next instruction.
    #[inline]
    pub const fn is_unconditional_terminator(&self) -> bool {
        match *self {
            Self::B { .. } | Self::Br { .. } | Self::Ret | Self::Brk { .. } | Self::Udf => true,
            Self::BCond { cond, .. } => matches!(cond, MCond::Al | MCond::Nv),
            Self::RawWord { word, .. } => enc::is_unconditional_terminator(word),
            Self::Mov { .. }
            | Self::MovImm { .. }
            | Self::MovSp { .. }
            | Self::AddImm { .. }
            | Self::SubImm { .. }
            | Self::AddReg { .. }
            | Self::SubReg { .. }
            | Self::AddExt { .. }
            | Self::SubExt { .. }
            | Self::LogicalImm { .. }
            | Self::LogicalReg { .. }
            | Self::Mvn { .. }
            | Self::ShiftVar { .. }
            | Self::Ubfm { .. }
            | Self::Sbfm { .. }
            | Self::Bfxil { .. }
            | Self::Extr { .. }
            | Self::Rbit { .. }
            | Self::Clz { .. }
            | Self::Rev { .. }
            | Self::Mul { .. }
            | Self::Madd { .. }
            | Self::Msub { .. }
            | Self::Umulh { .. }
            | Self::Smulh { .. }
            | Self::CarryAddSub { .. }
            | Self::Udiv { .. }
            | Self::Sdiv { .. }
            | Self::Umin { .. }
            | Self::Umax { .. }
            | Self::Smin { .. }
            | Self::Smax { .. }
            | Self::CmpImm { .. }
            | Self::CmpReg { .. }
            | Self::TstImm { .. }
            | Self::TstReg { .. }
            | Self::CcmpImm { .. }
            | Self::CcmpReg { .. }
            | Self::Csel { .. }
            | Self::Csinc { .. }
            | Self::Csinv { .. }
            | Self::Csneg { .. }
            | Self::Ldr { .. }
            | Self::Str { .. }
            | Self::Ldp { .. }
            | Self::Stp { .. }
            | Self::FpAlu { .. }
            | Self::FpCmp { .. }
            | Self::Fcsel { .. }
            | Self::VecAlu { .. }
            | Self::Adrp { .. }
            | Self::AddLo12 { .. }
            | Self::AdrTable { .. }
            | Self::Bl
            | Self::Blr { .. }
            | Self::Cbz { .. }
            | Self::Cbnz { .. }
            | Self::Tbz { .. }
            | Self::Tbnz { .. }
            | Self::Prfm { .. }
            | Self::Nop
            | Self::ParallelCopy { .. } => false,
        }
    }

    /// True if this instruction is a conditional branch to an internal [`MBlock`].
    #[inline]
    pub const fn is_conditional_branch(&self) -> bool {
        match *self {
            Self::BCond { cond, .. } => !matches!(cond, MCond::Al | MCond::Nv),
            Self::Cbz { .. } | Self::Cbnz { .. } | Self::Tbz { .. } | Self::Tbnz { .. } => true,
            _ => false,
        }
    }

    /// Returns the internal [`MBlock`] branch target, if any.
    #[inline]
    pub const fn branch_target(&self) -> Option<MBlock> {
        match *self {
            Self::B { target }
            | Self::BCond { target, .. }
            | Self::Cbz { target, .. }
            | Self::Cbnz { target, .. }
            | Self::Tbz { target, .. }
            | Self::Tbnz { target, .. } => Some(target),
            _ => None,
        }
    }
}

// -----------------------------------------------------------------------------
// Instruction flags & attached metadata notes
// -----------------------------------------------------------------------------

/// Per-instruction attribute bitset stored in [`MFunc::flags`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Debug)]
pub struct MInstFlags(pub u16);

impl MInstFlags {
    pub const NONE: Self = Self(0);
    pub const PINNED: Self = Self(1 << 0);
    pub const VOLATILE: Self = Self(1 << 1);
    pub const MAY_TRAP: Self = Self(1 << 2);
    pub const CALL: Self = Self(1 << 3);
    pub const TERMINATOR: Self = Self(1 << 4);
    pub const MEM_READ: Self = Self(1 << 5);
    pub const MEM_WRITE: Self = Self(1 << 6);
    pub const MEM_BARRIER: Self = Self(1 << 7);
    pub const REMATERIALIZABLE: Self = Self(1 << 8);
    pub const COLD: Self = Self(1 << 9);

    #[inline]
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    #[inline]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    #[inline]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// Kind of side-band metadata attached to an instruction via [`MFunc::notes`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum MNoteKind {
    /// Code relocation at this instruction's exact emitted byte offset.
    Reloc { kind: RelocKind, target: u32, addend: i64 },
    /// Trap-table row index anchored at this instruction's exact emitted byte offset.
    Trap { trap_idx: u32 },
    /// Unwind row index anchored at this instruction's exact emitted byte offset.
    Unwind { unwind_idx: u32 },
    /// Marks a volatile memory access that must not be elided, merged, or reordered.
    Volatile,
    /// Anchors an `adr` / jump-table base address site for table `table_id`.
    JumpTableAnchor { table_id: u32 },
}

/// Linked-list node in [`MFunc::notes`] allowing multiple notes per instruction with zero extra arrays.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct MNote {
    pub kind: MNoteKind,
    /// Next note index in [`MFunc::notes`], or [`NO_NOTE`].
    pub next: u32,
}

// -----------------------------------------------------------------------------
// Flat SoA Machine Function (`MFunc<'a>`)
// -----------------------------------------------------------------------------

/// Construction options for [`MFunc::new`] (Rule 83).
pub struct MFuncOpts<'a> {
    pub os: Os,
    pub is_ssa: bool,
    pub ret_live_gprs: u32,
    pub alloc: Alloc<'a>,
}

/// Flat Structure-of-Arrays (SoA) AArch64 Machine IR function.
pub struct MFunc<'a> {
    /// Typed machine operations (index = [`MInst`]).
    pub ops: Vec<MOp, Alloc<'a>>,
    /// Per-instruction attribute flags (parallel to `ops`).
    pub flags: Vec<MInstFlags, Alloc<'a>>,
    /// Per-instruction defined virtual flag ID, or [`NO_VFLAG`] (parallel to `ops`).
    pub vflag_def: Vec<u32, Alloc<'a>>,
    /// Per-instruction consumed virtual flag ID, or [`NO_VFLAG`] (parallel to `ops`).
    pub vflag_use: Vec<u32, Alloc<'a>>,
    /// Per-instruction head index into `notes`, or [`NO_NOTE`] (parallel to `ops`).
    pub note_idx: Vec<u32, Alloc<'a>>,
    /// Side-band metadata pool (relocs, traps, unwind rows, volatile markers, jump-table anchors).
    pub notes: Vec<MNote, Alloc<'a>>,
    /// Register class for each [`VReg`] (index = `VReg.0`).
    pub vreg_class: Vec<VRegClass, Alloc<'a>>,
    /// Per-block half-open instruction range `(start, end)` into `ops` (index = [`MBlock`]).
    pub block_insts: Vec<(u32, u32), Alloc<'a>>,
    /// Physical layout order of blocks for code emission.
    pub block_order: Vec<MBlock, Alloc<'a>>,
    /// Whether each block requires 16-byte instruction alignment (parallel to `block_insts`).
    pub block_align16: Vec<bool, Alloc<'a>>,
    /// CSR offsets for CFG predecessors (`block_preds_off[b]..block_preds_off[b + 1]`).
    pub block_preds_off: Vec<u32, Alloc<'a>>,
    /// CSR data for CFG predecessors.
    pub block_preds_data: Vec<MBlock, Alloc<'a>>,
    /// CSR offsets for CFG successors (`block_succs_off[b]..block_succs_off[b + 1]`).
    pub block_succs_off: Vec<u32, Alloc<'a>>,
    /// CSR data for CFG successors.
    pub block_succs_data: Vec<MBlock, Alloc<'a>>,
    /// `(dst, src)` register pairs referenced by [`MOp::ParallelCopy`].
    pub copy_pairs: Vec<(MReg, MReg), Alloc<'a>>,
    /// Target operating system (controls platform-reserved registers such as `X18` on macOS).
    pub os: Os,
    /// `true` prior to register allocation (`VReg` and `ParallelCopy` allowed); `false` post-RA.
    pub is_ssa: bool,
    /// Bitmask of GPRs live-out at `ret`.
    pub ret_live_gprs: u32,
}

impl<'a> MFunc<'a> {
    /// Creates an empty [`MFunc`] backed by `opts.alloc`.
    pub fn new(opts: MFuncOpts<'a>) -> Self {
        let a = opts.alloc;
        Self {
            ops: Vec::new_in(a),
            flags: Vec::new_in(a),
            vflag_def: Vec::new_in(a),
            vflag_use: Vec::new_in(a),
            note_idx: Vec::new_in(a),
            notes: Vec::new_in(a),
            vreg_class: Vec::new_in(a),
            block_insts: Vec::new_in(a),
            block_order: Vec::new_in(a),
            block_align16: Vec::new_in(a),
            block_preds_off: Vec::new_in(a),
            block_preds_data: Vec::new_in(a),
            block_succs_off: Vec::new_in(a),
            block_succs_data: Vec::new_in(a),
            copy_pairs: Vec::new_in(a),
            os: opts.os,
            is_ssa: opts.is_ssa,
            ret_live_gprs: opts.ret_live_gprs,
        }
    }

    /// Allocates a new virtual register of `class`.
    #[inline]
    pub fn alloc_vreg(&mut self, class: VRegClass) -> VReg {
        let id = u32_of(self.vreg_class.len()).unwrap_or(u32::MAX);
        self.vreg_class.push(class);
        VReg(id)
    }

    /// Allocates a new basic block with empty range `(0, 0)` and alignment preference `align16`.
    #[inline]
    pub fn new_block(&mut self, align16: bool) -> MBlock {
        let id = u32_of(self.block_insts.len()).unwrap_or(u32::MAX);
        self.block_insts.push((0, 0));
        self.block_align16.push(align16);
        MBlock(id)
    }

    /// Marks the start of `block` at the current instruction cursor and appends it to `block_order`.
    #[inline]
    pub fn begin_block(&mut self, block: MBlock) {
        let cur = u32_of(self.ops.len()).unwrap_or(u32::MAX);
        if let Some(slot) = self.block_insts.get_mut(to_idx(block.0)) {
            *slot = (cur, cur);
        }
        self.block_order.push(block);
    }

    /// Closes `block` at the current instruction cursor.
    #[inline]
    pub fn end_block(&mut self, block: MBlock) {
        let cur = u32_of(self.ops.len()).unwrap_or(u32::MAX);
        if let Some(slot) = self.block_insts.get_mut(to_idx(block.0)) {
            slot.1 = cur;
        }
    }

    /// Appends one machine instruction to all SoA columns in lockstep.
    #[inline]
    pub fn push_inst(&mut self, op: MOp, flags: MInstFlags) -> MInst {
        let idx = u32_of(self.ops.len()).unwrap_or(u32::MAX);
        self.ops.push(op);
        self.flags.push(flags);
        self.vflag_def.push(NO_VFLAG);
        self.vflag_use.push(NO_VFLAG);
        self.note_idx.push(NO_NOTE);
        MInst(idx)
    }

    /// Records the virtual flag ID defined by `inst`.
    #[inline]
    pub fn set_vflag_def(&mut self, inst: MInst, vflag: u32) {
        if let Some(slot) = self.vflag_def.get_mut(to_idx(inst.0)) {
            *slot = vflag;
        }
    }

    /// Records the virtual flag ID consumed by `inst`.
    #[inline]
    pub fn set_vflag_use(&mut self, inst: MInst, vflag: u32) {
        if let Some(slot) = self.vflag_use.get_mut(to_idx(inst.0)) {
            *slot = vflag;
        }
    }

    /// Attaches a side-band [`MNoteKind`] to `inst` (supports multiple notes per instruction).
    #[inline]
    pub fn attach_note(&mut self, inst: MInst, kind: MNoteKind) {
        let Some(head) = self.note_idx.get_mut(to_idx(inst.0)) else {
            return;
        };
        let next = *head;
        let idx = u32_of(self.notes.len()).unwrap_or(u32::MAX);
        self.notes.push(MNote { kind, next });
        *head = idx;
    }

    /// Appends a [`MOp::ParallelCopy`] instruction with `pairs` copied into `copy_pairs`.
    pub fn push_parallel_copy(&mut self, pairs: &[(MReg, MReg)], flags: MInstFlags) -> MInst {
        let start = u32_of(self.copy_pairs.len()).unwrap_or(u32::MAX);
        let len = u16::try_from(pairs.len()).unwrap_or(u16::MAX);
        for &pair in pairs.iter().take(usize::from(len)) {
            self.copy_pairs.push(pair);
        }
        self.push_inst(MOp::ParallelCopy { start, len }, flags)
    }

    /// Returns the instruction range of `block`, or `0..0` if out of bounds.
    #[inline]
    pub fn block_inst_range(&self, block: MBlock) -> Range<usize> {
        match self.block_insts.get(to_idx(block.0)) {
            Some(&(s, e)) => to_idx(s)..to_idx(e),
            None => 0..0,
        }
    }

    /// Returns the predecessor blocks of `block` from the CSR arrays.
    #[inline]
    pub fn block_preds(&self, block: MBlock) -> &[MBlock] {
        let b = to_idx(block.0);
        let (Some(&s), Some(&e)) = (self.block_preds_off.get(b), self.block_preds_off.get(b.saturating_add(1))) else {
            return &[];
        };
        self.block_preds_data.get(to_idx(s)..to_idx(e)).unwrap_or(&[])
    }

    /// Returns the successor blocks of `block` from the CSR arrays.
    #[inline]
    pub fn block_succs(&self, block: MBlock) -> &[MBlock] {
        let b = to_idx(block.0);
        let (Some(&s), Some(&e)) = (self.block_succs_off.get(b), self.block_succs_off.get(b.saturating_add(1))) else {
            return &[];
        };
        self.block_succs_data.get(to_idx(s)..to_idx(e)).unwrap_or(&[])
    }

    /// Rebuilds `block_succs_*` and `block_preds_*` CSR arrays from `block_order` and terminators.
    pub fn rebuild_cfg(&mut self, scratch: Alloc<'_>) {
        let num_blocks = self.block_insts.len();
        self.block_succs_off.clear();
        self.block_succs_data.clear();
        self.block_preds_off.clear();
        self.block_preds_data.clear();

        if num_blocks == 0 {
            return;
        }

        // Map each block to its next fallthrough block in `block_order`.
        let mut fallthrough: Vec<u32, Alloc<'_>> = Vec::with_capacity_in(num_blocks, scratch);
        for _ in 0..num_blocks {
            fallthrough.push(u32::MAX);
        }
        for pair in self.block_order.windows(2) {
            if let [cur, next] = *pair {
                let b = to_idx(cur.0);
                if let Some(slot) = fallthrough.get_mut(b) {
                    *slot = next.0;
                }
            }
        }

        let mut pred_counts: Vec<u32, Alloc<'_>> = Vec::with_capacity_in(num_blocks, scratch);
        for _ in 0..num_blocks {
            pred_counts.push(0);
        }

        // Pass 1: build `block_succs_off` / `block_succs_data` and count predecessors.
        self.block_succs_off.reserve(num_blocks + 1);
        for b in 0..num_blocks {
            self.block_succs_off.push(u32_of(self.block_succs_data.len()).unwrap_or(u32::MAX));
            let (start, end) = self.block_insts.get(b).copied().unwrap_or((0, 0));
            let s = to_idx(start);
            let e = to_idx(end).min(self.ops.len());
            let mut has_uncond = false;
            let succ_start = self.block_succs_data.len();

            if s < e
                && let Some(ops_slice) = self.ops.get(s..e)
            {
                for op in ops_slice {
                    if let Some(target) = op.branch_target() {
                        let t_idx = to_idx(target.0);
                        if t_idx < num_blocks
                            && !self.block_succs_data.get(succ_start..).unwrap_or(&[]).contains(&target)
                        {
                            self.block_succs_data.push(target);
                            if let Some(c) = pred_counts.get_mut(t_idx) {
                                *c = c.saturating_add(1);
                            }
                        }
                    }
                    if op.is_unconditional_terminator() {
                        has_uncond = true;
                    }
                }
            }
            if !has_uncond {
                let ft = fallthrough.get(b).copied().unwrap_or(u32::MAX);
                if ft != u32::MAX {
                    let ft_blk = MBlock(ft);
                    let ft_idx = to_idx(ft);
                    if ft_idx < num_blocks
                        && !self.block_succs_data.get(succ_start..).unwrap_or(&[]).contains(&ft_blk)
                    {
                        self.block_succs_data.push(ft_blk);
                        if let Some(c) = pred_counts.get_mut(ft_idx) {
                            *c = c.saturating_add(1);
                        }
                    }
                }
            }
        }
        self.block_succs_off.push(u32_of(self.block_succs_data.len()).unwrap_or(u32::MAX));

        // Pass 2: prefix-sum `pred_counts` into `block_preds_off` and fill `block_preds_data`.
        self.block_preds_off.reserve(num_blocks + 1);
        let mut cursor: Vec<u32, Alloc<'_>> = Vec::with_capacity_in(num_blocks, scratch);
        let mut total: u32 = 0;
        for &c in pred_counts.iter() {
            self.block_preds_off.push(total);
            cursor.push(total);
            total = total.saturating_add(c);
        }
        self.block_preds_off.push(total);

        let total_usize = to_idx(total);
        self.block_preds_data.reserve(total_usize);
        for _ in 0..total_usize {
            self.block_preds_data.push(MBlock(0));
        }
        for b in 0..num_blocks {
            let b_id = MBlock(u32_of(b).unwrap_or(u32::MAX));
            let ss = to_idx(self.block_succs_off.get(b).copied().unwrap_or(0));
            let se = to_idx(self.block_succs_off.get(b + 1).copied().unwrap_or(0));
            for &succ_blk in self.block_succs_data.get(ss..se).unwrap_or(&[]) {
                let succ = to_idx(succ_blk.0);
                if let Some(cur) = cursor.get_mut(succ) {
                    let dst = to_idx(*cur);
                    if let Some(slot) = self.block_preds_data.get_mut(dst) {
                        *slot = b_id;
                        *cur = cur.saturating_add(1);
                    }
                }
            }
        }
    }
}

// -----------------------------------------------------------------------------
// Structural Verifier (`mverify`)
// -----------------------------------------------------------------------------

/// Expected operand register class / physical-register role for verification.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OperandRole {
    /// Physical `Gpr(0..=30)` or `Xzr`; virtual `Gpr32 | Gpr64`.
    GprOrZr,
    /// Physical `Gpr(0..=30)` or `Sp`; virtual `Gpr32 | Gpr64`.
    GprOrSp,
    /// Physical `V(0..=31)`; virtual `Fp32`.
    Fp32,
    /// Physical `V(0..=31)`; virtual `Fp64`.
    Fp64,
    /// Physical `V(0..=31)`; virtual `Vec128`.
    Vec128,
    /// Physical `V(0..=31)`; virtual `Fp32 | Fp64 | Vec128` (scalar reduction output in `V`).
    FpOrVec,
}

/// Typed structural verification error returned by [`mverify`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MVerifyError {
    /// One of the parallel SoA arrays does not match `ops.len()` or `block_insts.len()`.
    ColumnLengthMismatch,
    /// `block_order` is empty.
    EmptyFunction,
    /// `block_order` references an out-of-bounds block.
    BlockOutOfBounds { block: MBlock },
    /// `block_order` lists the same block more than once.
    DuplicateBlockInOrder { block: MBlock },
    /// `block_insts[block]` has `start > end` or `end > ops.len()`.
    InvalidBlockRange { block: MBlock, start: u32, end: u32 },
    /// CSR predecessor or successor arrays are malformed.
    InvalidCfgCsr { block: MBlock },
    /// A branch references an out-of-bounds target block.
    InvalidBranchTarget { inst: MInst, target: MBlock },
    /// An instruction appears after an unconditional terminator, or a non-branch follows a conditional branch.
    InvalidTerminatorPlacement { block: MBlock, inst: MInst },
    /// The last block in `block_order` can fall off the end of the function.
    MissingExitTerminator { block: MBlock },
    /// An instruction's `note_idx` chain is out of bounds or cyclic.
    InvalidNoteChain { inst: MInst, note: u32 },
    /// A `VReg` appears when `func.is_ssa == false`.
    UnloweredVReg { inst: MInst, vreg: VReg },
    /// A `ParallelCopy` appears when `func.is_ssa == false`.
    UnloweredParallelCopy { inst: MInst },
    /// `ParallelCopy` range exceeds `copy_pairs.len()`.
    InvalidParallelCopyRange { inst: MInst },
    /// `VReg` index exceeds `vreg_class.len()`.
    VRegOutOfBounds { inst: MInst, vreg: VReg },
    /// `VReg` register class is incompatible with this operand position.
    VRegClassMismatch { inst: MInst, vreg: VReg, actual: VRegClass, expected: OperandRole },
    /// `PReg` carries an architecturally invalid register number (`Gpr(>30)` or `V(>31)`).
    InvalidPReg { inst: MInst, preg: PReg },
    /// `PReg` is reserved by the target OS (`X18` on `Os::MacOs`).
    ReservedPlatformReg { inst: MInst, preg: PReg },
    /// `PReg` is illegal for this operand role (e.g. `Sp` in `GprOrZr`, `Xzr` as memory base).
    IllegalPRegForOperand { inst: MInst, preg: PReg, expected: OperandRole },
    /// Instruction immediate, shift, bitfield, or addressing mode violates AArch64 ISA rules.
    InvalidEncodingOperand { inst: MInst },
    /// `vflag_def` is set on an instruction that does not write `NZCV`.
    VFlagDefOnNonFlagWriter { inst: MInst, vflag: u32 },
    /// `vflag_use` is set on an instruction that does not read `NZCV`.
    VFlagUseOnNonFlagReader { inst: MInst, vflag: u32 },
    /// Two live virtual flags overlap (a `vflag` was clobbered before its use).
    OverlappingLiveVFlags { block: MBlock, inst: MInst, expected: u32, active: Option<u32> },
    /// A `vflag_use` references a virtual flag not defined earlier in the block.
    UndefinedVFlagUse { block: MBlock, inst: MInst, vflag: u32 },
}

impl fmt::Display for MVerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

/// Verifies structural, ISA operand, register-class, platform-ABI, and single-live-`VFlag`
/// invariants of `func` (works both pre-RA `is_ssa == true` and post-RA `is_ssa == false`).
pub fn mverify(func: &MFunc<'_>) -> Result<(), MVerifyError> {
    let n_ops = func.ops.len();
    let n_blocks = func.block_insts.len();

    // 1. Lockstep SoA column lengths.
    if func.flags.len() != n_ops
        || func.vflag_def.len() != n_ops
        || func.vflag_use.len() != n_ops
        || func.note_idx.len() != n_ops
        || func.block_align16.len() != n_blocks
    {
        return Err(MVerifyError::ColumnLengthMismatch);
    }

    if func.block_order.is_empty() || n_blocks == 0 {
        return Err(MVerifyError::EmptyFunction);
    }

    // 2. Validate every block range.
    for (b, &(start, end)) in func.block_insts.iter().enumerate() {
        if start > end || to_idx(end) > n_ops {
            return Err(MVerifyError::InvalidBlockRange {
                block: MBlock(u32_of(b).unwrap_or(u32::MAX)),
                start,
                end,
            });
        }
    }

    // 3. Validate `block_order` bounds and uniqueness with zero heap allocation (chunked stack bitset).
    let mut base: usize = 0;
    while base < n_blocks {
        let mut seen = [0u64; 64]; // 4096 blocks per chunk
        let limit = base.saturating_add(4096);
        for &blk in func.block_order.iter() {
            let idx = to_idx(blk.0);
            if idx >= n_blocks {
                return Err(MVerifyError::BlockOutOfBounds { block: blk });
            }
            if idx >= base && idx < limit {
                let rel = idx - base;
                let word = rel >> 6;
                let bit = 1u64 << (rel & 63);
                if let Some(slot) = seen.get_mut(word) {
                    if (*slot & bit) != 0 {
                        return Err(MVerifyError::DuplicateBlockInOrder { block: blk });
                    }
                    *slot |= bit;
                }
            }
        }
        base = limit;
    }

    // 4. Validate CSR predecessor/successor arrays if populated.
    verify_csr(&func.block_preds_off, &func.block_preds_data, n_blocks)?;
    verify_csr(&func.block_succs_off, &func.block_succs_data, n_blocks)?;

    // 5. Validate note chains for all instructions.
    let n_notes = func.notes.len();
    for (i, &head) in func.note_idx.iter().enumerate() {
        let mut cur = head;
        let mut steps: usize = 0;
        while cur != NO_NOTE {
            let ci = to_idx(cur);
            let Some(note) = func.notes.get(ci).filter(|_| steps < n_notes) else {
                return Err(MVerifyError::InvalidNoteChain {
                    inst: MInst(u32_of(i).unwrap_or(u32::MAX)),
                    note: cur,
                });
            };
            cur = note.next;
            steps += 1;
        }
    }

    // 6. Validate each block in `block_order`: terminators, operands, and single-live-VFlag.
    let last_order_pos = func.block_order.len() - 1;
    for (order_pos, &blk) in func.block_order.iter().enumerate() {
        let Some(&(start, end)) = func.block_insts.get(to_idx(blk.0)) else {
            return Err(MVerifyError::BlockOutOfBounds { block: blk });
        };
        let s = to_idx(start);
        let e = to_idx(end);

        let mut seen_cond_branch = false;
        let mut seen_uncond_term = false;
        let mut active_vflag: Option<u32> = None;

        for i in s..e {
            let inst = MInst(u32_of(i).unwrap_or(u32::MAX));
            let Some(op) = func.ops.get(i) else {
                return Err(MVerifyError::InvalidBlockRange { block: blk, start, end });
            };

            // Terminator placement checks.
            if seen_uncond_term {
                return Err(MVerifyError::InvalidTerminatorPlacement { block: blk, inst });
            }
            if seen_cond_branch && !op.is_conditional_branch() && !op.is_unconditional_terminator() {
                return Err(MVerifyError::InvalidTerminatorPlacement { block: blk, inst });
            }
            if let Some(target) = op.branch_target()
                && to_idx(target.0) >= n_blocks
            {
                return Err(MVerifyError::InvalidBranchTarget { inst, target });
            }
            if op.is_conditional_branch() {
                seen_cond_branch = true;
            }
            if op.is_unconditional_terminator() {
                seen_uncond_term = true;
            }

            // Instruction operand & register legality checks.
            verify_op(func, inst, op)?;

            // Virtual flag single-live-NZCV check: uses BEFORE defs (e.g. `Ccmp` reads then writes).
            let vuse = func.vflag_use.get(i).copied().unwrap_or(NO_VFLAG);
            if vuse != NO_VFLAG {
                if !op.reads_flags() {
                    return Err(MVerifyError::VFlagUseOnNonFlagReader { inst, vflag: vuse });
                }
                if active_vflag != Some(vuse) {
                    let was_defined_earlier = func.vflag_def.get(s..i).unwrap_or(&[]).contains(&vuse);
                    if was_defined_earlier {
                        return Err(MVerifyError::OverlappingLiveVFlags {
                            block: blk,
                            inst,
                            expected: vuse,
                            active: active_vflag,
                        });
                    }
                    return Err(MVerifyError::UndefinedVFlagUse {
                        block: blk,
                        inst,
                        vflag: vuse,
                    });
                }
            }

            let vdef = func.vflag_def.get(i).copied().unwrap_or(NO_VFLAG);
            if vdef != NO_VFLAG {
                if !op.writes_flags() {
                    return Err(MVerifyError::VFlagDefOnNonFlagWriter { inst, vflag: vdef });
                }
                active_vflag = Some(vdef);
            } else if op.writes_flags() {
                // Physical flag clobber (e.g. `Bl`, `Blr`, unannotated `Cmp`, `RawWord { writes_flags }`).
                active_vflag = None;
            }
        }

        if order_pos == last_order_pos && !seen_uncond_term {
            return Err(MVerifyError::MissingExitTerminator { block: blk });
        }
    }

    Ok(())
}

fn verify_csr(off: &[u32], data: &[MBlock], n_blocks: usize) -> Result<(), MVerifyError> {
    if off.is_empty() && data.is_empty() {
        return Ok(());
    }
    if off.len() != n_blocks + 1 || off.get(n_blocks).copied().map(to_idx) != Some(data.len()) {
        return Err(MVerifyError::InvalidCfgCsr { block: MBlock(0) });
    }
    for b in 0..n_blocks {
        let block = MBlock(u32_of(b).unwrap_or(u32::MAX));
        let (Some(&start_u), Some(&end_u)) = (off.get(b), off.get(b + 1)) else {
            return Err(MVerifyError::InvalidCfgCsr { block });
        };
        if start_u > end_u {
            return Err(MVerifyError::InvalidCfgCsr { block });
        }
        let s = to_idx(start_u);
        let e = to_idx(end_u);
        let Some(targets) = data.get(s..e) else {
            return Err(MVerifyError::InvalidCfgCsr { block });
        };
        for &target in targets {
            if to_idx(target.0) >= n_blocks {
                return Err(MVerifyError::InvalidCfgCsr { block });
            }
        }
    }
    Ok(())
}

fn verify_reg(func: &MFunc<'_>, inst: MInst, reg: MReg, role: OperandRole) -> Result<(), MVerifyError> {
    match reg {
        MReg::V(vreg) => {
            if !func.is_ssa {
                return Err(MVerifyError::UnloweredVReg { inst, vreg });
            }
            let Some(&actual) = func.vreg_class.get(to_idx(vreg.0)) else {
                return Err(MVerifyError::VRegOutOfBounds { inst, vreg });
            };
            let ok = match role {
                OperandRole::GprOrZr | OperandRole::GprOrSp => actual.is_gpr(),
                OperandRole::Fp32 => actual == VRegClass::Fp32,
                OperandRole::Fp64 => actual == VRegClass::Fp64,
                OperandRole::Vec128 => actual == VRegClass::Vec128,
                OperandRole::FpOrVec => actual.is_fp_or_vec(),
            };
            if !ok {
                return Err(MVerifyError::VRegClassMismatch {
                    inst,
                    vreg,
                    actual,
                    expected: role,
                });
            }
            Ok(())
        }
        MReg::P(preg) => {
            if !preg.is_arch_valid() {
                return Err(MVerifyError::InvalidPReg { inst, preg });
            }
            if preg.is_os_reserved(func.os) {
                return Err(MVerifyError::ReservedPlatformReg { inst, preg });
            }
            let ok = match role {
                OperandRole::GprOrZr => preg.gpr_or_zr_enc().is_some(),
                OperandRole::GprOrSp => preg.gpr_or_sp_enc().is_some(),
                OperandRole::Fp32 | OperandRole::Fp64 | OperandRole::Vec128 | OperandRole::FpOrVec => {
                    preg.vec_enc().is_some()
                }
            };
            if !ok {
                return Err(MVerifyError::IllegalPRegForOperand {
                    inst,
                    preg,
                    expected: role,
                });
            }
            Ok(())
        }
    }
}

fn mem_value_role(size: MemSize) -> OperandRole {
    match size {
        MemSize::B8 | MemSize::H16 | MemSize::W32 | MemSize::X64 => OperandRole::GprOrZr,
        MemSize::S32 => OperandRole::Fp32,
        MemSize::D64 => OperandRole::Fp64,
        MemSize::Q128 => OperandRole::Vec128,
    }
}

fn verify_shift(inst: MInst, shift: Option<MShift>, wide: bool) -> Result<(), MVerifyError> {
    if let Some(sh) = shift {
        let max = if wide { 63 } else { 31 };
        if sh.amount() > max {
            return Err(MVerifyError::InvalidEncodingOperand { inst });
        }
    }
    Ok(())
}

fn verify_addr_mode(
    func: &MFunc<'_>,
    inst: MInst,
    addr: AddrMode,
    size: MemSize,
    is_gpr_load_dst: Option<MReg>,
) -> Result<(), MVerifyError> {
    match addr {
        AddrMode::BaseOff { base, off } => {
            verify_reg(func, inst, base, OperandRole::GprOrSp)?;
            let bytes = i32::try_from(size.byte_size()).unwrap_or(16);
            let scaled_ok = off >= 0 && (off % bytes) == 0 && (off / bytes) <= 4095;
            let unscaled_ok = (-256..=255).contains(&off);
            if !scaled_ok && !unscaled_ok {
                return Err(MVerifyError::InvalidEncodingOperand { inst });
            }
        }
        AddrMode::ScaledIdx { base, idx, shift } => {
            verify_reg(func, inst, base, OperandRole::GprOrSp)?;
            verify_reg(func, inst, idx, OperandRole::GprOrZr)?;
            if shift != 0 && shift != size.scale_shift() {
                return Err(MVerifyError::InvalidEncodingOperand { inst });
            }
        }
        AddrMode::PostIdx { base, off } | AddrMode::PreIdx { base, off } => {
            verify_reg(func, inst, base, OperandRole::GprOrSp)?;
            if !(-256..=255).contains(&off) {
                return Err(MVerifyError::InvalidEncodingOperand { inst });
            }
            if let (Some(MReg::P(PReg::Gpr(d))), MReg::P(PReg::Gpr(b))) = (is_gpr_load_dst, base)
                && d == b
            {
                return Err(MVerifyError::InvalidEncodingOperand { inst });
            }
        }
    }
    Ok(())
}

fn verify_pair_mem(
    func: &MFunc<'_>,
    inst: MInst,
    r0: MReg,
    r1: MReg,
    base: MReg,
    off: i16,
    size: MemSize,
    post_idx: bool,
    pre_idx: bool,
    is_load: bool,
) -> Result<(), MVerifyError> {
    if matches!(size, MemSize::B8 | MemSize::H16) || (post_idx && pre_idx) {
        return Err(MVerifyError::InvalidEncodingOperand { inst });
    }
    let val_role = mem_value_role(size);
    verify_reg(func, inst, r0, val_role)?;
    verify_reg(func, inst, r1, val_role)?;
    verify_reg(func, inst, base, OperandRole::GprOrSp)?;

    let bytes = i32::try_from(size.byte_size()).unwrap_or(8);
    let off32 = i32::from(off);
    if (off32 % bytes) != 0 {
        return Err(MVerifyError::InvalidEncodingOperand { inst });
    }
    let scaled = off32 / bytes;
    if !(-64..=63).contains(&scaled) {
        return Err(MVerifyError::InvalidEncodingOperand { inst });
    }
    if is_load && !size.is_fp_or_vec() {
        if let (MReg::P(PReg::Gpr(a)), MReg::P(PReg::Gpr(b))) = (r0, r1)
            && a == b
        {
            return Err(MVerifyError::InvalidEncodingOperand { inst });
        }
        if (post_idx || pre_idx)
            && let MReg::P(PReg::Gpr(b)) = base
            && (matches!(r0, MReg::P(PReg::Gpr(a)) if a == b)
                || matches!(r1, MReg::P(PReg::Gpr(a)) if a == b))
        {
            return Err(MVerifyError::InvalidEncodingOperand { inst });
        }
    }
    Ok(())
}

fn verify_op(func: &MFunc<'_>, inst: MInst, op: &MOp) -> Result<(), MVerifyError> {
    match *op {
        MOp::Mov { dst: a, src: b, .. }
        | MOp::Rbit { dst: a, rn: b, .. }
        | MOp::Clz { dst: a, rn: b, .. }
        | MOp::Rev { dst: a, rn: b, .. }
        | MOp::TstReg { rn: a, rm: b, .. } => {
            verify_reg(func, inst, a, OperandRole::GprOrZr)?;
            verify_reg(func, inst, b, OperandRole::GprOrZr)?;
        }
        MOp::MovImm { dst: reg, .. }
        | MOp::Adrp { dst: reg }
        | MOp::AdrTable { dst: reg, .. }
        | MOp::Blr { target: reg }
        | MOp::Br { target: reg }
        | MOp::Cbz { rt: reg, .. }
        | MOp::Cbnz { rt: reg, .. } => {
            verify_reg(func, inst, reg, OperandRole::GprOrZr)?;
        }
        MOp::MovSp { dst, src } | MOp::AddLo12 { dst, rn: src } => {
            verify_reg(func, inst, dst, OperandRole::GprOrSp)?;
            verify_reg(func, inst, src, OperandRole::GprOrSp)?;
        }
        MOp::AddImm { dst, rn, imm12, sets_flags, .. }
        | MOp::SubImm { dst, rn, imm12, sets_flags, .. } => {
            let dst_role = if sets_flags { OperandRole::GprOrZr } else { OperandRole::GprOrSp };
            verify_reg(func, inst, dst, dst_role)?;
            verify_reg(func, inst, rn, OperandRole::GprOrSp)?;
            if imm12 > 0xfff {
                return Err(MVerifyError::InvalidEncodingOperand { inst });
            }
        }
        MOp::AddReg { dst, rn, rm, shift, wide, .. }
        | MOp::SubReg { dst, rn, rm, shift, wide, .. }
        | MOp::LogicalReg { dst, rn, rm, shift, wide, .. } => {
            verify_reg(func, inst, dst, OperandRole::GprOrZr)?;
            verify_reg(func, inst, rn, OperandRole::GprOrZr)?;
            verify_reg(func, inst, rm, OperandRole::GprOrZr)?;
            verify_shift(inst, shift, wide)?;
        }
        MOp::AddExt { dst, rn, rm, sets_flags, .. }
        | MOp::SubExt { dst, rn, rm, sets_flags, .. } => {
            let dst_role = if sets_flags { OperandRole::GprOrZr } else { OperandRole::GprOrSp };
            verify_reg(func, inst, dst, dst_role)?;
            verify_reg(func, inst, rn, OperandRole::GprOrSp)?;
            verify_reg(func, inst, rm, OperandRole::GprOrZr)?;
        }
        MOp::LogicalImm { dst, rn, imm, kind, wide } => {
            let dst_role = match kind {
                LogicalKind::Ands => OperandRole::GprOrZr,
                LogicalKind::And | LogicalKind::Orr | LogicalKind::Eor => OperandRole::GprOrSp,
            };
            verify_reg(func, inst, dst, dst_role)?;
            verify_reg(func, inst, rn, OperandRole::GprOrZr)?;
            let width = if wide { 64 } else { 32 };
            if enc::logical_imm(imm, width).is_none() {
                return Err(MVerifyError::InvalidEncodingOperand { inst });
            }
        }
        MOp::Mvn { dst, rm, shift, wide } => {
            verify_reg(func, inst, dst, OperandRole::GprOrZr)?;
            verify_reg(func, inst, rm, OperandRole::GprOrZr)?;
            verify_shift(inst, shift, wide)?;
        }
        MOp::ShiftVar { dst, rn, rm, .. }
        | MOp::Mul { dst, rn, rm, .. }
        | MOp::Umulh { dst, rn, rm }
        | MOp::Smulh { dst, rn, rm }
        | MOp::CarryAddSub { dst, rn, rm, .. }
        | MOp::Udiv { dst, rn, rm, .. }
        | MOp::Sdiv { dst, rn, rm, .. }
        | MOp::Umin { dst, rn, rm, .. }
        | MOp::Umax { dst, rn, rm, .. }
        | MOp::Smin { dst, rn, rm, .. }
        | MOp::Smax { dst, rn, rm, .. }
        | MOp::Csel { dst, rn, rm, .. }
        | MOp::Csinc { dst, rn, rm, .. }
        | MOp::Csinv { dst, rn, rm, .. }
        | MOp::Csneg { dst, rn, rm, .. } => {
            verify_reg(func, inst, dst, OperandRole::GprOrZr)?;
            verify_reg(func, inst, rn, OperandRole::GprOrZr)?;
            verify_reg(func, inst, rm, OperandRole::GprOrZr)?;
        }
        MOp::Madd { dst, rn, rm, ra, .. } | MOp::Msub { dst, rn, rm, ra, .. } => {
            verify_reg(func, inst, dst, OperandRole::GprOrZr)?;
            verify_reg(func, inst, rn, OperandRole::GprOrZr)?;
            verify_reg(func, inst, rm, OperandRole::GprOrZr)?;
            verify_reg(func, inst, ra, OperandRole::GprOrZr)?;
        }
        MOp::Ubfm { dst, rn, immr, imms, wide } | MOp::Sbfm { dst, rn, immr, imms, wide } => {
            verify_reg(func, inst, dst, OperandRole::GprOrZr)?;
            verify_reg(func, inst, rn, OperandRole::GprOrZr)?;
            let max = if wide { 63 } else { 31 };
            if immr > max || imms > max {
                return Err(MVerifyError::InvalidEncodingOperand { inst });
            }
        }
        MOp::Bfxil { dst, rn, lsb, width, wide } => {
            verify_reg(func, inst, dst, OperandRole::GprOrZr)?;
            verify_reg(func, inst, rn, OperandRole::GprOrZr)?;
            let max_bits = if wide { 64u16 } else { 32u16 };
            if width == 0 || u16::from(lsb) + u16::from(width) > max_bits {
                return Err(MVerifyError::InvalidEncodingOperand { inst });
            }
        }
        MOp::Extr { dst, rn, rm, lsb, wide } => {
            verify_reg(func, inst, dst, OperandRole::GprOrZr)?;
            verify_reg(func, inst, rn, OperandRole::GprOrZr)?;
            verify_reg(func, inst, rm, OperandRole::GprOrZr)?;
            let max = if wide { 63 } else { 31 };
            if lsb > max {
                return Err(MVerifyError::InvalidEncodingOperand { inst });
            }
        }
        MOp::CmpImm { rn, imm12, .. } => {
            verify_reg(func, inst, rn, OperandRole::GprOrSp)?;
            if imm12 > 0xfff {
                return Err(MVerifyError::InvalidEncodingOperand { inst });
            }
        }
        MOp::CmpReg { rn, rm, shift, wide } => {
            verify_reg(func, inst, rn, OperandRole::GprOrZr)?;
            verify_reg(func, inst, rm, OperandRole::GprOrZr)?;
            verify_shift(inst, shift, wide)?;
        }
        MOp::TstImm { rn, imm, wide } => {
            verify_reg(func, inst, rn, OperandRole::GprOrZr)?;
            let width = if wide { 64 } else { 32 };
            if enc::logical_imm(imm, width).is_none() {
                return Err(MVerifyError::InvalidEncodingOperand { inst });
            }
        }
        MOp::CcmpImm { rn, imm5, nzcv, .. } => {
            verify_reg(func, inst, rn, OperandRole::GprOrZr)?;
            if imm5 > 31 || nzcv > 15 {
                return Err(MVerifyError::InvalidEncodingOperand { inst });
            }
        }
        MOp::CcmpReg { rn, rm, nzcv, .. } => {
            verify_reg(func, inst, rn, OperandRole::GprOrZr)?;
            verify_reg(func, inst, rm, OperandRole::GprOrZr)?;
            if nzcv > 15 {
                return Err(MVerifyError::InvalidEncodingOperand { inst });
            }
        }
        MOp::Ldr { dst, addr, size, signed } => {
            if signed && (size == MemSize::X64 || size.is_fp_or_vec()) {
                return Err(MVerifyError::InvalidEncodingOperand { inst });
            }
            verify_reg(func, inst, dst, mem_value_role(size))?;
            let gpr_dst = if size.is_fp_or_vec() { None } else { Some(dst) };
            verify_addr_mode(func, inst, addr, size, gpr_dst)?;
        }
        MOp::Str { src, addr, size } => {
            verify_reg(func, inst, src, mem_value_role(size))?;
            verify_addr_mode(func, inst, addr, size, None)?;
        }
        MOp::Ldp { dst0, dst1, base, off, size, post_idx, pre_idx } => {
            verify_pair_mem(func, inst, dst0, dst1, base, off, size, post_idx, pre_idx, true)?;
        }
        MOp::Stp { src0, src1, base, off, size, post_idx, pre_idx } => {
            verify_pair_mem(func, inst, src0, src1, base, off, size, post_idx, pre_idx, false)?;
        }
        MOp::FpAlu { op: fp_op, dst, rn, rm, ra, double } => {
            verify_fp_alu(func, inst, fp_op, dst, rn, rm, ra, double)?;
        }
        MOp::FpCmp { rn, rm, double } => {
            let role = if double { OperandRole::Fp64 } else { OperandRole::Fp32 };
            verify_reg(func, inst, rn, role)?;
            verify_reg(func, inst, rm, role)?;
        }
        MOp::Fcsel { dst, rn, rm, double, .. } => {
            let role = if double { OperandRole::Fp64 } else { OperandRole::Fp32 };
            verify_reg(func, inst, dst, role)?;
            verify_reg(func, inst, rn, role)?;
            verify_reg(func, inst, rm, role)?;
        }
        MOp::VecAlu { op: vec_op, dst, rn, rm, elem_size } => {
            verify_vec_alu(func, inst, vec_op, dst, rn, rm, elem_size)?;
        }
        MOp::Tbz { rt, bit, .. } | MOp::Tbnz { rt, bit, .. } => {
            verify_reg(func, inst, rt, OperandRole::GprOrZr)?;
            if bit > 63 {
                return Err(MVerifyError::InvalidEncodingOperand { inst });
            }
        }
        MOp::ParallelCopy { start, len } => {
            if !func.is_ssa {
                return Err(MVerifyError::UnloweredParallelCopy { inst });
            }
            let s = to_idx(start);
            let e = s.saturating_add(usize::from(len));
            let Some(pairs) = func.copy_pairs.get(s..e) else {
                return Err(MVerifyError::InvalidParallelCopyRange { inst });
            };
            for &(dst, src) in pairs {
                verify_copy_reg(func, inst, dst)?;
                verify_copy_reg(func, inst, src)?;
            }
        }
        MOp::RawWord { defs_gpr, uses_gpr, .. } => {
            if func.os == Os::MacOs && (((defs_gpr | uses_gpr) & (1u32 << 18)) != 0) {
                return Err(MVerifyError::ReservedPlatformReg { inst, preg: PReg::X18 });
            }
        }
        MOp::Prfm { base, imm12 } => {
            verify_reg(func, inst, base, OperandRole::GprOrSp)?;
            if imm12 >= 4096 {
                return Err(MVerifyError::InvalidEncodingOperand { inst });
            }
        }
        MOp::Bl | MOp::B { .. } | MOp::BCond { .. } | MOp::Ret | MOp::Brk { .. } | MOp::Udf | MOp::Nop => {}
    }
    Ok(())
}

fn verify_copy_reg(func: &MFunc<'_>, inst: MInst, reg: MReg) -> Result<(), MVerifyError> {
    match reg {
        MReg::V(vreg) => {
            if !func.is_ssa {
                return Err(MVerifyError::UnloweredVReg { inst, vreg });
            }
            let Some(&cls) = func.vreg_class.get(to_idx(vreg.0)) else {
                return Err(MVerifyError::VRegOutOfBounds { inst, vreg });
            };
            if cls.is_flags() {
                return Err(MVerifyError::VRegClassMismatch {
                    inst,
                    vreg,
                    actual: cls,
                    expected: OperandRole::GprOrZr,
                });
            }
            Ok(())
        }
        MReg::P(preg) => {
            if !preg.is_arch_valid() {
                return Err(MVerifyError::InvalidPReg { inst, preg });
            }
            if preg.is_os_reserved(func.os) {
                return Err(MVerifyError::ReservedPlatformReg { inst, preg });
            }
            if matches!(preg, PReg::Nzcv) {
                return Err(MVerifyError::IllegalPRegForOperand {
                    inst,
                    preg,
                    expected: OperandRole::GprOrZr,
                });
            }
            Ok(())
        }
    }
}

fn verify_fp_alu(
    func: &MFunc<'_>,
    inst: MInst,
    fp_op: FpAluOp,
    dst: MReg,
    rn: MReg,
    rm: Option<MReg>,
    ra: Option<MReg>,
    double: bool,
) -> Result<(), MVerifyError> {
    let fp_role = if double { OperandRole::Fp64 } else { OperandRole::Fp32 };
    match fp_op {
        FpAluOp::Add | FpAluOp::Sub | FpAluOp::Mul | FpAluOp::Div => {
            verify_reg(func, inst, dst, fp_role)?;
            verify_reg(func, inst, rn, fp_role)?;
            let Some(m) = rm else {
                return Err(MVerifyError::InvalidEncodingOperand { inst });
            };
            verify_reg(func, inst, m, fp_role)?;
        }
        FpAluOp::Madd | FpAluOp::Msub => {
            verify_reg(func, inst, dst, fp_role)?;
            verify_reg(func, inst, rn, fp_role)?;
            let (Some(m), Some(a)) = (rm, ra) else {
                return Err(MVerifyError::InvalidEncodingOperand { inst });
            };
            verify_reg(func, inst, m, fp_role)?;
            verify_reg(func, inst, a, fp_role)?;
        }
        FpAluOp::Mov
        | FpAluOp::Neg
        | FpAluOp::Abs
        | FpAluOp::Sqrt
        | FpAluOp::RintN
        | FpAluOp::RintP
        | FpAluOp::RintM
        | FpAluOp::RintZ
        | FpAluOp::RintA => {
            verify_reg(func, inst, dst, fp_role)?;
            verify_reg(func, inst, rn, fp_role)?;
        }
        FpAluOp::CvtS2D => {
            verify_reg(func, inst, dst, OperandRole::Fp64)?;
            verify_reg(func, inst, rn, OperandRole::Fp32)?;
        }
        FpAluOp::CvtD2S => {
            verify_reg(func, inst, dst, OperandRole::Fp32)?;
            verify_reg(func, inst, rn, OperandRole::Fp64)?;
        }
        FpAluOp::MovFromGpr | FpAluOp::Scvtf { .. } | FpAluOp::Ucvtf { .. } => {
            verify_reg(func, inst, dst, fp_role)?;
            verify_reg(func, inst, rn, OperandRole::GprOrZr)?;
        }
        FpAluOp::MovToGpr | FpAluOp::Fcvtzs { .. } | FpAluOp::Fcvtzu { .. } => {
            verify_reg(func, inst, dst, OperandRole::GprOrZr)?;
            verify_reg(func, inst, rn, fp_role)?;
        }
    }
    Ok(())
}

fn verify_vec_alu(
    func: &MFunc<'_>,
    inst: MInst,
    vec_op: VecAluOp,
    dst: MReg,
    rn: MReg,
    rm: Option<MReg>,
    elem_size: u8,
) -> Result<(), MVerifyError> {
    match vec_op {
        VecAluOp::Zero => {
            verify_reg(func, inst, dst, OperandRole::Vec128)?;
        }
        VecAluOp::Mov | VecAluOp::Not => {
            verify_reg(func, inst, dst, OperandRole::Vec128)?;
            verify_reg(func, inst, rn, OperandRole::Vec128)?;
        }
        VecAluOp::And | VecAluOp::Orr | VecAluOp::Eor | VecAluOp::Bsl | VecAluOp::Bit | VecAluOp::Bif => {
            verify_reg(func, inst, dst, OperandRole::Vec128)?;
            verify_reg(func, inst, rn, OperandRole::Vec128)?;
            let Some(m) = rm else {
                return Err(MVerifyError::InvalidEncodingOperand { inst });
            };
            verify_reg(func, inst, m, OperandRole::Vec128)?;
        }
        VecAluOp::Add
        | VecAluOp::Sub
        | VecAluOp::Sqadd
        | VecAluOp::Uqadd
        | VecAluOp::Sqsub
        | VecAluOp::Uqsub
        | VecAluOp::Cmeq
        | VecAluOp::Cmgt
        | VecAluOp::Cmge
        | VecAluOp::Cmhi
        | VecAluOp::Cmhs => {
            if elem_size > 3 {
                return Err(MVerifyError::InvalidEncodingOperand { inst });
            }
            verify_reg(func, inst, dst, OperandRole::Vec128)?;
            verify_reg(func, inst, rn, OperandRole::Vec128)?;
            let Some(m) = rm else {
                return Err(MVerifyError::InvalidEncodingOperand { inst });
            };
            verify_reg(func, inst, m, OperandRole::Vec128)?;
        }
        VecAluOp::Mul
        | VecAluOp::Mla
        | VecAluOp::Mls
        | VecAluOp::Smin
        | VecAluOp::Smax
        | VecAluOp::Umin
        | VecAluOp::Umax => {
            if elem_size > 2 {
                return Err(MVerifyError::InvalidEncodingOperand { inst });
            }
            verify_reg(func, inst, dst, OperandRole::Vec128)?;
            verify_reg(func, inst, rn, OperandRole::Vec128)?;
            let Some(m) = rm else {
                return Err(MVerifyError::InvalidEncodingOperand { inst });
            };
            verify_reg(func, inst, m, OperandRole::Vec128)?;
        }
        VecAluOp::Fadd
        | VecAluOp::Fsub
        | VecAluOp::Fmul
        | VecAluOp::Fmla
        | VecAluOp::Fmls
        | VecAluOp::FaddpV => {
            if elem_size != 2 && elem_size != 3 {
                return Err(MVerifyError::InvalidEncodingOperand { inst });
            }
            verify_reg(func, inst, dst, OperandRole::Vec128)?;
            verify_reg(func, inst, rn, OperandRole::Vec128)?;
            let Some(m) = rm else {
                return Err(MVerifyError::InvalidEncodingOperand { inst });
            };
            verify_reg(func, inst, m, OperandRole::Vec128)?;
        }
        VecAluOp::DupGen => {
            if elem_size > 3 {
                return Err(MVerifyError::InvalidEncodingOperand { inst });
            }
            verify_reg(func, inst, dst, OperandRole::Vec128)?;
            verify_reg(func, inst, rn, OperandRole::GprOrZr)?;
        }
        VecAluOp::DupElem0 => {
            if elem_size != 2 && elem_size != 3 {
                return Err(MVerifyError::InvalidEncodingOperand { inst });
            }
            verify_reg(func, inst, dst, OperandRole::Vec128)?;
            verify_reg(func, inst, rn, OperandRole::FpOrVec)?;
        }
        VecAluOp::Addv => {
            if elem_size > 2 {
                return Err(MVerifyError::InvalidEncodingOperand { inst });
            }
            verify_reg(func, inst, dst, OperandRole::FpOrVec)?;
            verify_reg(func, inst, rn, OperandRole::Vec128)?;
        }
        VecAluOp::AddpD | VecAluOp::UminvB => {
            verify_reg(func, inst, dst, OperandRole::FpOrVec)?;
            verify_reg(func, inst, rn, OperandRole::Vec128)?;
        }
        VecAluOp::FaddpScalar => {
            if elem_size != 2 && elem_size != 3 {
                return Err(MVerifyError::InvalidEncodingOperand { inst });
            }
            verify_reg(func, inst, dst, OperandRole::FpOrVec)?;
            verify_reg(func, inst, rn, OperandRole::Vec128)?;
        }
        VecAluOp::UmovLane { lane } => {
            if elem_size > 3 || u32::from(lane) >= (16u32 >> u32::from(elem_size)) {
                return Err(MVerifyError::InvalidEncodingOperand { inst });
            }
            verify_reg(func, inst, dst, OperandRole::GprOrZr)?;
            verify_reg(func, inst, rn, OperandRole::Vec128)?;
        }
        VecAluOp::Ext { imm4 } => {
            if imm4 > 15 {
                return Err(MVerifyError::InvalidEncodingOperand { inst });
            }
            verify_reg(func, inst, dst, OperandRole::Vec128)?;
            verify_reg(func, inst, rn, OperandRole::Vec128)?;
            let Some(m) = rm else {
                return Err(MVerifyError::InvalidEncodingOperand { inst });
            };
            verify_reg(func, inst, m, OperandRole::Vec128)?;
        }
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// 1-Pass Binary Encoder (`mencode`) with Branch14 / Branch19 Range Relaxation
// -----------------------------------------------------------------------------

/// Emitted relocation with its exact byte offset in `EncodedMFunc::words`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RelocAt {
    pub byte_off: u32,
    pub kind: RelocKind,
    pub target: u32,
    pub addend: i64,
}

/// Emitted trap row with its exact byte offset in `EncodedMFunc::words`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TrapAt {
    pub byte_off: u32,
    pub trap_idx: u32,
}

/// Emitted unwind row with its exact byte offset in `EncodedMFunc::words`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct UnwindAt {
    pub byte_off: u32,
    pub unwind_idx: u32,
}

/// Emitted jump-table `adr` anchor word index.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TableAdrAt {
    pub table_id: u32,
    pub word_idx: u32,
}

/// Binary output of [`mencode`].
pub struct EncodedMFunc<'a> {
    pub words: Vec<u32, Alloc<'a>>,
    pub block_word_off: Vec<u32, Alloc<'a>>,
    pub reloc_byte_offs: Vec<RelocAt, Alloc<'a>>,
    pub trap_byte_offs: Vec<TrapAt, Alloc<'a>>,
    pub unwind_byte_offs: Vec<UnwindAt, Alloc<'a>>,
    pub table_adr_word_idxs: Vec<TableAdrAt, Alloc<'a>>,
    pub relaxed_branches: u32,
}

/// Typed encoding error returned by [`mencode`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MEncodeError {
    /// Cannot encode an SSA function (`func.is_ssa == true`); run register allocation first.
    StillInSsa,
    /// Structural verification failed prior to encoding.
    VerifyFailed(MVerifyError),
    /// A virtual register remained after register allocation.
    UnloweredVReg { inst: MInst, vreg: VReg },
    /// A `ParallelCopy` remained after register allocation.
    UnloweredParallelCopy { inst: MInst },
    /// Physical register is illegal for this operand role.
    IllegalPReg { inst: MInst, preg: PReg },
    /// Immediate or addressing mode is out of architectural range.
    InvalidOperand { inst: MInst },
    /// Branch target exceeds `±128 MiB` (`Imm26`) even after relaxation.
    BranchOutOfRange { inst: MInst, target: MBlock, word_delta: i64 },
    /// Branch relaxation failed to converge within [`MAX_RELAX_ROUNDS`].
    RelaxationDidNotConverge,
    /// Emitted code exceeds `u32` byte-offset addressing limit.
    CodeTooLarge,
}

impl fmt::Display for MEncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

/// True iff `blk` contains at least one non-`Nop` instruction and its last non-`Nop`
/// instruction is an unconditional terminator (so execution cannot fall through into the next
/// layout block).
#[inline]
fn block_ends_with_uncond_term(func: &MFunc<'_>, blk: MBlock) -> bool {
    let b = to_idx(blk.0);
    let Some(&(start_u, end_u)) = func.block_insts.get(b) else {
        return false;
    };
    let start = to_idx(start_u);
    let end = to_idx(end_u).min(func.ops.len());
    let Some(ops) = func.ops.get(start..end) else {
        return false;
    };
    for &op in ops.iter().rev() {
        if matches!(op, MOp::Nop) {
            continue;
        }
        return op.is_unconditional_terminator();
    }
    false
}

/// True iff `func.block_order[ord_pos]` should be padded with alignment `NOP`s to a 16-byte
/// boundary:
/// - Never pad the entry block (`ord_pos == 0`).
/// - Always align a true loop header (`func.block_align16[blk]` AND `is_backedge_target[blk]`),
///   even across a pre-header fallthrough, so hot loop headers land cleanly on Apple Silicon /
///   Graviton 16-byte fetch packets.
/// - Never align when neither `func.block_align16[blk]` nor `is_backedge_target[blk]` holds.
/// - Otherwise (`block_align16` without a backward branch, or a backward branch targeting a
///   non-loop block such as a diamond merge jumped to from a cold tail), pad ONLY if the
///   immediately preceding non-empty block ends with an unconditional terminator (zero executed
///   fallthrough `NOP`s ever).
#[inline]
fn should_align16_block(func: &MFunc<'_>, is_backedge_target: &[bool], ord_pos: usize) -> bool {
    if ord_pos == 0 {
        return false;
    }
    let Some(&blk) = func.block_order.get(ord_pos) else {
        return false;
    };
    let b = to_idx(blk.0);
    let is_hdr = func.block_align16.get(b).copied().unwrap_or(false);
    let is_back = is_backedge_target.get(b).copied().unwrap_or(false);
    if is_hdr && is_back {
        return true;
    }
    if !is_hdr && !is_back {
        return false;
    }
    for &prev_blk in func.block_order.get(..ord_pos).unwrap_or(&[]).iter().rev() {
        let Some(&(start_u, end_u)) = func.block_insts.get(to_idx(prev_blk.0)) else {
            continue;
        };
        if start_u < end_u {
            return block_ends_with_uncond_term(func, prev_blk);
        }
    }
    false
}

/// Encodes a post-RA [`MFunc`] into AArch64 instruction words via [`crate::enc`], automatically
/// relaxing out-of-range `Imm14` (`±32 KiB`) and `Imm19` (`±1 MiB`) conditional branches and
/// recording exact relocation, trap, unwind, and jump-table offsets directly from [`MNote`]s.
pub fn mencode<'a>(func: &MFunc<'a>, alloc: Alloc<'a>) -> Result<EncodedMFunc<'a>, MEncodeError> {
    if func.is_ssa {
        return Err(MEncodeError::StillInSsa);
    }
    mverify(func).map_err(MEncodeError::VerifyFailed)?;

    let n_ops = func.ops.len();
    let n_blocks = func.block_insts.len();

    let mut relaxed: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(n_ops, alloc);
    let mut inst_word_off: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(n_ops, alloc);
    for _ in 0..n_ops {
        relaxed.push(false);
        inst_word_off.push(0);
    }
    let mut block_word_off: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(n_blocks, alloc);
    let mut block_ord_pos: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(n_blocks, alloc);
    let mut is_backedge_target: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(n_blocks, alloc);
    for _ in 0..n_blocks {
        block_word_off.push(0);
        block_ord_pos.push(u32::MAX);
        is_backedge_target.push(false);
    }
    for (ord_pos, &blk) in func.block_order.iter().enumerate() {
        let b = to_idx(blk.0);
        if let Some(slot) = block_ord_pos.get_mut(b) {
            *slot = u32_of(ord_pos).unwrap_or(u32::MAX);
        }
    }
    for (ord_pos, &blk) in func.block_order.iter().enumerate() {
        let b = to_idx(blk.0);
        let Some(&(start, end)) = func.block_insts.get(b) else {
            continue;
        };
        for &op in func.ops.get(to_idx(start)..to_idx(end)).unwrap_or(&[]) {
            let target = match op {
                MOp::B { target }
                | MOp::BCond { target, .. }
                | MOp::Cbz { target, .. }
                | MOp::Cbnz { target, .. }
                | MOp::Tbz { target, .. }
                | MOp::Tbnz { target, .. } => Some(target),
                _ => None,
            };
            if let Some(t) = target {
                let tb = to_idx(t.0);
                if block_ord_pos
                    .get(tb)
                    .is_some_and(|&pos| usize::try_from(pos).unwrap_or(usize::MAX) <= ord_pos)
                    && let Some(flag) = is_backedge_target.get_mut(tb)
                {
                    *flag = true;
                }
            }
        }
    }

    // Bounded monotone fixed-point layout + `Imm14`/`Imm19` range relaxation.
    let mut total_words: u32 = 0;
    let mut converged = false;
    for _ in 0..MAX_RELAX_ROUNDS {
        let mut cursor: u32 = 0;
        for (ord_pos, &blk) in func.block_order.iter().enumerate() {
            let b = to_idx(blk.0);
            if should_align16_block(func, is_backedge_target.as_slice(), ord_pos) {
                let pad = (4 - (cursor & 3)) & 3;
                cursor = cursor.checked_add(pad).ok_or(MEncodeError::CodeTooLarge)?;
            }
            if let Some(slot) = block_word_off.get_mut(b) {
                *slot = cursor;
            }
            let Some(&(start, end)) = func.block_insts.get(b) else {
                continue;
            };
            for i in to_idx(start)..to_idx(end) {
                if let Some(slot) = inst_word_off.get_mut(i) {
                    *slot = cursor;
                }
                let Some(op) = func.ops.get(i) else {
                    continue;
                };
                let is_relaxed = relaxed.get(i).copied().unwrap_or(false);
                let count = inst_word_count(op, is_relaxed, MInst(u32_of(i).unwrap_or(u32::MAX)))?;
                cursor = cursor.checked_add(count).ok_or(MEncodeError::CodeTooLarge)?;
            }
        }
        total_words = cursor;

        let mut changed = false;
        for &blk in func.block_order.iter() {
            let Some(&(start, end)) = func.block_insts.get(to_idx(blk.0)) else {
                continue;
            };
            for i in to_idx(start)..to_idx(end) {
                if relaxed.get(i).copied().unwrap_or(false) {
                    continue;
                }
                let Some(&op) = func.ops.get(i) else {
                    continue;
                };
                let inst_off = inst_word_off.get(i).copied().unwrap_or(0);
                let needs_relax = match op {
                    MOp::Tbz { target, .. } | MOp::Tbnz { target, .. } => {
                        let target_off = block_word_off.get(to_idx(target.0)).copied().unwrap_or(0);
                        let delta = i64::from(target_off) - i64::from(inst_off);
                        !(IMM14_MIN_WORDS..=IMM14_MAX_WORDS).contains(&delta)
                    }
                    MOp::BCond { cond, target } if !matches!(cond, MCond::Al | MCond::Nv) => {
                        let target_off = block_word_off.get(to_idx(target.0)).copied().unwrap_or(0);
                        let delta = i64::from(target_off) - i64::from(inst_off);
                        !(IMM19_MIN_WORDS..=IMM19_MAX_WORDS).contains(&delta)
                    }
                    MOp::Cbz { target, .. } | MOp::Cbnz { target, .. } => {
                        let target_off = block_word_off.get(to_idx(target.0)).copied().unwrap_or(0);
                        let delta = i64::from(target_off) - i64::from(inst_off);
                        !(IMM19_MIN_WORDS..=IMM19_MAX_WORDS).contains(&delta)
                    }
                    _ => false,
                };
                if needs_relax && let Some(slot) = relaxed.get_mut(i) {
                    *slot = true;
                    changed = true;
                }
            }
        }

        if !changed {
            converged = true;
            break;
        }
    }

    if !converged {
        return Err(MEncodeError::RelaxationDidNotConverge);
    }

    let _total_bytes = total_words.checked_mul(4).ok_or(MEncodeError::CodeTooLarge)?;

    let mut words: Vec<u32, Alloc<'a>> = Vec::with_capacity_in(to_idx(total_words), alloc);
    let mut reloc_byte_offs: Vec<RelocAt, Alloc<'a>> = Vec::new_in(alloc);
    let mut trap_byte_offs: Vec<TrapAt, Alloc<'a>> = Vec::new_in(alloc);
    let mut unwind_byte_offs: Vec<UnwindAt, Alloc<'a>> = Vec::new_in(alloc);
    let mut table_adr_word_idxs: Vec<TableAdrAt, Alloc<'a>> = Vec::new_in(alloc);
    let mut relaxed_branches: u32 = 0;

    // Final 1-pass emission: every block offset & displacement is exact!
    for (ord_pos, &blk) in func.block_order.iter().enumerate() {
        let b = to_idx(blk.0);
        if should_align16_block(func, is_backedge_target.as_slice(), ord_pos) {
            let cur = u32_of(words.len()).ok_or(MEncodeError::CodeTooLarge)?;
            let pad = (4 - (cur & 3)) & 3;
            for _ in 0..pad {
                words.push(enc::nop());
            }
        }

        let Some(&(start, end)) = func.block_insts.get(b) else {
            continue;
        };
        for i in to_idx(start)..to_idx(end) {
            let inst = MInst(u32_of(i).unwrap_or(u32::MAX));
            let word_idx = u32_of(words.len()).ok_or(MEncodeError::CodeTooLarge)?;
            let byte_off = word_idx.checked_mul(4).ok_or(MEncodeError::CodeTooLarge)?;
            let Some(op) = func.ops.get(i) else {
                continue;
            };

            // Record exact metadata from attached notes (preserving attachment order isn't required;
            // each instruction's byte offset is exact!).
            let is_adr_table = matches!(op, MOp::AdrTable { .. });
            if let MOp::AdrTable { table_id, .. } = *op {
                table_adr_word_idxs.push(TableAdrAt { table_id, word_idx });
            }
            let mut cur_note = func.note_idx.get(i).copied().unwrap_or(NO_NOTE);
            while cur_note != NO_NOTE {
                let Some(&note) = func.notes.get(to_idx(cur_note)) else {
                    break;
                };
                match note.kind {
                    MNoteKind::Reloc { kind, target, addend } => {
                        reloc_byte_offs.push(RelocAt {
                            byte_off,
                            kind,
                            target,
                            addend,
                        });
                    }
                    MNoteKind::Trap { trap_idx } => {
                        trap_byte_offs.push(TrapAt { byte_off, trap_idx });
                    }
                    MNoteKind::Unwind { unwind_idx } => {
                        unwind_byte_offs.push(UnwindAt { byte_off, unwind_idx });
                    }
                    MNoteKind::JumpTableAnchor { table_id } => {
                        if !is_adr_table {
                            table_adr_word_idxs.push(TableAdrAt { table_id, word_idx });
                        }
                    }
                    MNoteKind::Volatile => {}
                }
                cur_note = note.next;
            }

            let is_relaxed = relaxed.get(i).copied().unwrap_or(false);
            if is_relaxed {
                relaxed_branches = relaxed_branches.saturating_add(1);
            }
            encode_inst(op, inst, word_idx, is_relaxed, &block_word_off, &mut words)?;
        }
    }

    Ok(EncodedMFunc {
        words,
        block_word_off,
        reloc_byte_offs,
        trap_byte_offs,
        unwind_byte_offs,
        table_adr_word_idxs,
        relaxed_branches,
    })
}

fn req_gpr_or_zr(inst: MInst, reg: MReg) -> Result<u32, MEncodeError> {
    match reg {
        MReg::V(vreg) => Err(MEncodeError::UnloweredVReg { inst, vreg }),
        MReg::P(preg) => preg.gpr_or_zr_enc().ok_or(MEncodeError::IllegalPReg { inst, preg }),
    }
}

fn req_gpr_or_sp(inst: MInst, reg: MReg) -> Result<u32, MEncodeError> {
    match reg {
        MReg::V(vreg) => Err(MEncodeError::UnloweredVReg { inst, vreg }),
        MReg::P(preg) => preg.gpr_or_sp_enc().ok_or(MEncodeError::IllegalPReg { inst, preg }),
    }
}

fn req_vec(inst: MInst, reg: MReg) -> Result<u32, MEncodeError> {
    match reg {
        MReg::V(vreg) => Err(MEncodeError::UnloweredVReg { inst, vreg }),
        MReg::P(preg) => preg.vec_enc().ok_or(MEncodeError::IllegalPReg { inst, preg }),
    }
}

/// Encodes `MovImm` into 1..=4 AArch64 words using `movz`/`movn`+`movk` (or 1-word `orr #imm`
/// when `rd != XZR` and `movz`/`movn` would require 2+ words).
fn encode_mov_imm_words(rd: u32, imm: u64, wide: bool) -> ([u32; 4], u32) {
    let mut out = [0u32; 4];
    if !wide {
        let val = imm & 0xffff_ffff;
        let lo = chunk16(val, 0);
        let hi = chunk16(val, 1);
        if hi == 0 {
            out[0] = enc::sf(false, enc::movz(rd, lo, 0));
            return (out, 1);
        }
        if lo == 0 {
            out[0] = enc::sf(false, enc::movz(rd, hi, 1));
            return (out, 1);
        }
        if hi == 0xffff {
            out[0] = enc::sf(false, enc::movn(rd, (!lo) & 0xffff, 0));
            return (out, 1);
        }
        if lo == 0xffff {
            out[0] = enc::sf(false, enc::movn(rd, (!hi) & 0xffff, 1));
            return (out, 1);
        }
        if rd != enc::ZR
            && let Some(nrs) = enc::logical_imm(val, 32)
        {
            out[0] = enc::sf(false, enc::orr_imm_nrs(rd, enc::ZR, trunc_u32(nrs)));
            return (out, 1);
        }
        out[0] = enc::sf(false, enc::movz(rd, lo, 0));
        out[1] = enc::sf(false, enc::movk(rd, hi, 1));
        return (out, 2);
    }

    let chunks = [
        chunk16(imm, 0),
        chunk16(imm, 1),
        chunk16(imm, 2),
        chunk16(imm, 3),
    ];
    let mut zero_cnt: u32 = 0;
    let mut ones_cnt: u32 = 0;
    for &c in &chunks {
        if c == 0 {
            zero_cnt += 1;
        }
        if c == 0xffff {
            ones_cnt += 1;
        }
    }

    if zero_cnt == 4 {
        out[0] = enc::movz(rd, 0, 0);
        return (out, 1);
    }
    if ones_cnt == 4 {
        out[0] = enc::movn(rd, 0, 0);
        return (out, 1);
    }
    if zero_cnt == 3 {
        for (hw, &c) in chunks.iter().enumerate() {
            if c != 0 {
                out[0] = enc::movz(rd, c, u32_of(hw).unwrap_or(0));
                return (out, 1);
            }
        }
    }
    if ones_cnt == 3 {
        for (hw, &c) in chunks.iter().enumerate() {
            if c != 0xffff {
                out[0] = enc::movn(rd, (!c) & 0xffff, u32_of(hw).unwrap_or(0));
                return (out, 1);
            }
        }
    }
    if rd != enc::ZR
        && let Some(nrs) = enc::logical_imm(imm, 64)
    {
        out[0] = enc::orr_imm_nrs(rd, enc::ZR, trunc_u32(nrs));
        return (out, 1);
    }

    let mut len: usize = 0;
    if ones_cnt > zero_cnt {
        let mut first = true;
        for (hw, &c) in chunks.iter().enumerate() {
            if c != 0xffff {
                let hw32 = u32_of(hw).unwrap_or(0);
                let word = if first {
                    first = false;
                    enc::movn(rd, (!c) & 0xffff, hw32)
                } else {
                    enc::movk(rd, c, hw32)
                };
                if let Some(slot) = out.get_mut(len) {
                    *slot = word;
                    len += 1;
                }
            }
        }
    } else {
        let mut first = true;
        for (hw, &c) in chunks.iter().enumerate() {
            if c != 0 {
                let hw32 = u32_of(hw).unwrap_or(0);
                let word = if first {
                    first = false;
                    enc::movz(rd, c, hw32)
                } else {
                    enc::movk(rd, c, hw32)
                };
                if let Some(slot) = out.get_mut(len) {
                    *slot = word;
                    len += 1;
                }
            }
        }
    }
    (out, u32_of(len).unwrap_or(1))
}

fn inst_word_count(op: &MOp, is_relaxed: bool, inst: MInst) -> Result<u32, MEncodeError> {
    match *op {
        MOp::MovImm { dst, imm, wide } => {
            let rd = req_gpr_or_zr(inst, dst)?;
            let (_, count) = encode_mov_imm_words(rd, imm, wide);
            Ok(count)
        }
        MOp::BCond { .. } | MOp::Cbz { .. } | MOp::Cbnz { .. } | MOp::Tbz { .. } | MOp::Tbnz { .. } => {
            Ok(if is_relaxed { 2 } else { 1 })
        }
        MOp::ParallelCopy { .. } => Err(MEncodeError::UnloweredParallelCopy { inst }),
        _ => Ok(1),
    }
}

fn branch_delta_i32(
    inst: MInst,
    target: MBlock,
    from_word: u32,
    block_word_off: &[u32],
    min_words: i64,
    max_words: i64,
) -> Result<i32, MEncodeError> {
    let target_word = block_word_off
        .get(to_idx(target.0))
        .copied()
        .ok_or(MEncodeError::InvalidOperand { inst })?;
    let delta = i64::from(target_word) - i64::from(from_word);
    if !(min_words..=max_words).contains(&delta) {
        return Err(MEncodeError::BranchOutOfRange {
            inst,
            target,
            word_delta: delta,
        });
    }
    i32::try_from(delta).map_err(|_| MEncodeError::BranchOutOfRange {
        inst,
        target,
        word_delta: delta,
    })
}

fn shift_bits(shift: Option<MShift>) -> u32 {
    match shift {
        Some(sh) => (sh.kind_bits() << 22) | (u8_to_u32(sh.amount()) << 10),
        None => 0,
    }
}

fn encode_mem_single(
    inst: MInst,
    rt: u32,
    addr: AddrMode,
    size: MemSize,
    is_load: bool,
    signed: bool,
) -> Result<u32, MEncodeError> {
    // Helper turning unsigned `opc = 01` into signed 64-bit destination `opc = 10` (`ldrsb`/`ldrsh`/`ldrsw`).
    let apply_signed = |w: u32| -> u32 {
        if signed {
            (w & !(0b11u32 << 22)) | (0b10u32 << 22)
        } else {
            w
        }
    };

    match addr {
        AddrMode::BaseOff { base, off } => {
            let rn = req_gpr_or_sp(inst, base)?;
            let bytes = i32::try_from(size.byte_size()).unwrap_or(16);
            if size == MemSize::Q128 {
                if off >= 0 && (off % 16) == 0 && (off / 16) <= 4095 {
                    let imm12 = u32::try_from(off / 16).map_err(|_| MEncodeError::InvalidOperand { inst })?;
                    return Ok(if is_load {
                        enc::ldr_q(rt, rn, imm12)
                    } else {
                        enc::str_q(rt, rn, imm12)
                    });
                }
                if (-256..=255).contains(&off) {
                    return Ok(if is_load {
                        enc::ldur_q(rt, rn, off)
                    } else {
                        enc::stur_q(rt, rn, off)
                    });
                }
                return Err(MEncodeError::InvalidOperand { inst });
            }

            let enc_sz = size.to_enc_size().ok_or(MEncodeError::InvalidOperand { inst })?;
            let mut word = if off >= 0 && (off % bytes) == 0 && (off / bytes) <= 4095 {
                let imm12 = u32::try_from(off / bytes).map_err(|_| MEncodeError::InvalidOperand { inst })?;
                if is_load {
                    enc::ldr(enc_sz, rt, rn, imm12)
                } else {
                    enc::str(enc_sz, rt, rn, imm12)
                }
            } else if (-256..=255).contains(&off) {
                if is_load {
                    enc::ldur(enc_sz, rt, rn, off)
                } else {
                    enc::stur(enc_sz, rt, rn, off)
                }
            } else {
                return Err(MEncodeError::InvalidOperand { inst });
            };
            if size.is_fp_or_vec() {
                word = enc::fp_access(word);
            } else if is_load {
                word = apply_signed(word);
            }
            Ok(word)
        }
        AddrMode::ScaledIdx { base, idx, shift } => {
            let rn = req_gpr_or_sp(inst, base)?;
            let rm = req_gpr_or_zr(inst, idx)?;
            let scaled = shift != 0 && shift == size.scale_shift();
            if size == MemSize::Q128 {
                return Ok(if is_load {
                    enc::ldr_q_reg(rt, rn, rm, scaled)
                } else {
                    enc::str_q_reg(rt, rn, rm, scaled)
                });
            }
            let enc_sz = size.to_enc_size().ok_or(MEncodeError::InvalidOperand { inst })?;
            let mut word = if is_load {
                if scaled {
                    enc::ldr_reg_scaled(enc_sz, rt, rn, rm)
                } else {
                    enc::ldr_reg(enc_sz, rt, rn, rm)
                }
            } else if scaled {
                enc::str_reg_scaled(enc_sz, rt, rn, rm)
            } else {
                enc::str_reg(enc_sz, rt, rn, rm)
            };
            if size.is_fp_or_vec() {
                word = enc::fp_access(word);
            } else if is_load {
                word = apply_signed(word);
            }
            Ok(word)
        }
        AddrMode::PostIdx { base, off } | AddrMode::PreIdx { base, off } => {
            let rn = req_gpr_or_sp(inst, base)?;
            let simm9 = i32::from(off);
            if !(-256..=255).contains(&simm9) {
                return Err(MEncodeError::InvalidOperand { inst });
            }
            let is_post = matches!(addr, AddrMode::PostIdx { .. });
            // ARM64 immediate writeback bits (11:10): post-index = 01 (`0x0400`), pre-index = 11 (`0x0c00`).
            let wb_bits: u32 = if is_post { 0x0400 } else { 0x0c00 };
            if size == MemSize::Q128 {
                return Ok(match (is_load, is_post) {
                    (true, true) => enc::ldr_q_post(rt, rn, simm9),
                    (false, true) => enc::str_q_post(rt, rn, simm9),
                    (true, false) => enc::ldur_q(rt, rn, simm9) | wb_bits,
                    (false, false) => enc::stur_q(rt, rn, simm9) | wb_bits,
                });
            }
            if size == MemSize::X64 && is_post {
                return Ok(if is_load {
                    enc::ldr_post(rt, rn, simm9)
                } else {
                    enc::str_post(rt, rn, simm9)
                });
            }
            let enc_sz = size.to_enc_size().ok_or(MEncodeError::InvalidOperand { inst })?;
            let mut word = if is_load {
                enc::ldur(enc_sz, rt, rn, simm9) | wb_bits
            } else {
                enc::stur(enc_sz, rt, rn, simm9) | wb_bits
            };
            if size.is_fp_or_vec() {
                word = enc::fp_access(word);
            } else if is_load {
                word = apply_signed(word);
            }
            Ok(word)
        }
    }
}

fn encode_pair_mem(
    inst: MInst,
    rt0: u32,
    rt1: u32,
    base: MReg,
    off: i16,
    size: MemSize,
    post_idx: bool,
    pre_idx: bool,
    is_load: bool,
) -> Result<u32, MEncodeError> {
    let rn = req_gpr_or_sp(inst, base)?;
    let off32 = i32::from(off);
    // Bits 24:23 in `LDP`/`STP`: post-index = `01`, signed-offset = `10`, pre-index = `11`.
    let set_pair_mode = |off_word: u32| -> u32 {
        if post_idx {
            (off_word & !(0b11u32 << 23)) | (0b01u32 << 23)
        } else if pre_idx {
            (off_word & !(0b11u32 << 23)) | (0b11u32 << 23)
        } else {
            off_word
        }
    };

    match (size, is_load, post_idx, pre_idx) {
        (MemSize::X64, true, false, false) => Ok(enc::ldp_off(rt0, rt1, rn, off32)),
        (MemSize::X64, true, true, false) => Ok(enc::ldp_post(rt0, rt1, rn, off32)),
        (MemSize::X64, true, false, true) => Ok(set_pair_mode(enc::ldp_off(rt0, rt1, rn, off32))),
        (MemSize::X64, false, false, false) => Ok(enc::stp_off(rt0, rt1, rn, off32)),
        (MemSize::X64, false, true, false) => Ok(enc::stp_post(rt0, rt1, rn, off32)),
        (MemSize::X64, false, false, true) => Ok(enc::stp_pre(rt0, rt1, rn, off32)),

        (MemSize::W32, true, _, _) => Ok(set_pair_mode(enc::ldp_w_off(rt0, rt1, rn, off32))),
        (MemSize::W32, false, _, _) => Ok(set_pair_mode(enc::stp_w_off(rt0, rt1, rn, off32))),

        (MemSize::S32, true, _, _) => Ok(set_pair_mode(enc::ldp_s_off(rt0, rt1, rn, off32))),
        (MemSize::S32, false, _, _) => Ok(set_pair_mode(enc::stp_s_off(rt0, rt1, rn, off32))),

        (MemSize::D64, true, false, false) => Ok(enc::ldp_d_off(rt0, rt1, rn, off32)),
        (MemSize::D64, true, true, false) => Ok(enc::ldp_post_d(rt0, rt1, rn, off32)),
        (MemSize::D64, true, false, true) => Ok(set_pair_mode(enc::ldp_d_off(rt0, rt1, rn, off32))),
        (MemSize::D64, false, false, false) => Ok(enc::stp_d_off(rt0, rt1, rn, off32)),
        (MemSize::D64, false, true, false) => Ok(enc::stp_post_d(rt0, rt1, rn, off32)),
        (MemSize::D64, false, false, true) => Ok(enc::stp_pre_d(rt0, rt1, rn, off32)),

        (MemSize::Q128, true, false, false) => Ok(enc::ldp_q_off(rt0, rt1, rn, off32 / 16)),
        (MemSize::Q128, true, true, false) => Ok(enc::ldp_q_post(rt0, rt1, rn, off32 / 16)),
        (MemSize::Q128, true, false, true) => Ok(set_pair_mode(enc::ldp_q_off(rt0, rt1, rn, off32 / 16))),
        (MemSize::Q128, false, false, false) => Ok(enc::stp_q_off(rt0, rt1, rn, off32 / 16)),
        (MemSize::Q128, false, true, false) => Ok(enc::stp_q_post(rt0, rt1, rn, off32 / 16)),
        (MemSize::Q128, false, false, true) => Ok(set_pair_mode(enc::stp_q_off(rt0, rt1, rn, off32 / 16))),

        _ => Err(MEncodeError::InvalidOperand { inst }),
    }
}

fn encode_fp_alu(
    inst: MInst,
    fp_op: FpAluOp,
    dst: MReg,
    rn: MReg,
    rm: Option<MReg>,
    ra: Option<MReg>,
    double: bool,
) -> Result<u32, MEncodeError> {
    let ft = if double { Ftype::D } else { Ftype::S };
    match fp_op {
        FpAluOp::Add => {
            let rd = req_vec(inst, dst)?;
            let n = req_vec(inst, rn)?;
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::fbin(0b0010, double, rd, n, m))
        }
        FpAluOp::Sub => {
            let rd = req_vec(inst, dst)?;
            let n = req_vec(inst, rn)?;
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::fbin(0b0011, double, rd, n, m))
        }
        FpAluOp::Mul => {
            let rd = req_vec(inst, dst)?;
            let n = req_vec(inst, rn)?;
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::fbin(0b0000, double, rd, n, m))
        }
        FpAluOp::Div => {
            let rd = req_vec(inst, dst)?;
            let n = req_vec(inst, rn)?;
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::fbin(0b0001, double, rd, n, m))
        }
        FpAluOp::Madd => {
            let rd = req_vec(inst, dst)?;
            let n = req_vec(inst, rn)?;
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            let a = req_vec(inst, ra.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::fmadd(ft, rd, n, m, a))
        }
        FpAluOp::Msub => {
            let rd = req_vec(inst, dst)?;
            let n = req_vec(inst, rn)?;
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            let a = req_vec(inst, ra.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::fmsub(ft, rd, n, m, a))
        }
        FpAluOp::Mov => Ok(enc::fmov_reg(double, req_vec(inst, dst)?, req_vec(inst, rn)?)),
        FpAluOp::Neg => Ok(enc::fneg(double, req_vec(inst, dst)?, req_vec(inst, rn)?)),
        FpAluOp::Abs => Ok(enc::fp1_op(0b00_0001, double, req_vec(inst, dst)?, req_vec(inst, rn)?)),
        FpAluOp::Sqrt => Ok(enc::fp1_op(0b00_0011, double, req_vec(inst, dst)?, req_vec(inst, rn)?)),
        FpAluOp::RintN => Ok(enc::fp1_op(0b00_1000, double, req_vec(inst, dst)?, req_vec(inst, rn)?)),
        FpAluOp::RintP => Ok(enc::fp1_op(0b00_1001, double, req_vec(inst, dst)?, req_vec(inst, rn)?)),
        FpAluOp::RintM => Ok(enc::fp1_op(0b00_1010, double, req_vec(inst, dst)?, req_vec(inst, rn)?)),
        FpAluOp::RintZ => Ok(enc::fp1_op(0b00_1011, double, req_vec(inst, dst)?, req_vec(inst, rn)?)),
        FpAluOp::RintA => Ok(enc::fp1_op(0b00_1100, double, req_vec(inst, dst)?, req_vec(inst, rn)?)),
        FpAluOp::CvtS2D => Ok(enc::fcvt(true, req_vec(inst, dst)?, req_vec(inst, rn)?)),
        FpAluOp::CvtD2S => Ok(enc::fcvt(false, req_vec(inst, dst)?, req_vec(inst, rn)?)),
        FpAluOp::MovFromGpr => {
            let rd = req_vec(inst, dst)?;
            let n = req_gpr_or_zr(inst, rn)?;
            Ok(if double { enc::fmov_to_d(rd, n) } else { enc::fmov_to_s(rd, n) })
        }
        FpAluOp::MovToGpr => {
            let rd = req_gpr_or_zr(inst, dst)?;
            let n = req_vec(inst, rn)?;
            Ok(if double { enc::fmov_from_d(rd, n) } else { enc::fmov_from_s(rd, n) })
        }
        FpAluOp::Scvtf { int_wide } => {
            Ok(enc::cvtf(true, int_wide, double, req_vec(inst, dst)?, req_gpr_or_zr(inst, rn)?))
        }
        FpAluOp::Ucvtf { int_wide } => {
            Ok(enc::cvtf(false, int_wide, double, req_vec(inst, dst)?, req_gpr_or_zr(inst, rn)?))
        }
        FpAluOp::Fcvtzs { int_wide } => {
            Ok(enc::fcvtz(true, int_wide, double, req_gpr_or_zr(inst, dst)?, req_vec(inst, rn)?))
        }
        FpAluOp::Fcvtzu { int_wide } => {
            Ok(enc::fcvtz(false, int_wide, double, req_gpr_or_zr(inst, dst)?, req_vec(inst, rn)?))
        }
    }
}

fn encode_vec_alu(
    inst: MInst,
    vec_op: VecAluOp,
    dst: MReg,
    rn: MReg,
    rm: Option<MReg>,
    elem_size: u8,
) -> Result<u32, MEncodeError> {
    let sz = u8_to_u32(elem_size);
    let double = elem_size == 3;
    match vec_op {
        VecAluOp::Zero => Ok(enc::movi_zero_v(req_vec(inst, dst)?)),
        VecAluOp::Mov => {
            let n = req_vec(inst, rn)?;
            Ok(enc::orr_v(req_vec(inst, dst)?, n, n))
        }
        VecAluOp::Not => Ok(enc::not_v(req_vec(inst, dst)?, req_vec(inst, rn)?)),
        VecAluOp::And => {
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::and_v(true, req_vec(inst, dst)?, req_vec(inst, rn)?, m))
        }
        VecAluOp::Orr => {
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::orr_v(req_vec(inst, dst)?, req_vec(inst, rn)?, m))
        }
        VecAluOp::Eor => {
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::eor_v(req_vec(inst, dst)?, req_vec(inst, rn)?, m))
        }
        VecAluOp::Bsl => {
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::bsl_v(req_vec(inst, dst)?, req_vec(inst, rn)?, m))
        }
        VecAluOp::Bit => {
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::bit_v(req_vec(inst, dst)?, req_vec(inst, rn)?, m))
        }
        VecAluOp::Bif => {
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::bif_v(req_vec(inst, dst)?, req_vec(inst, rn)?, m))
        }
        VecAluOp::Add => {
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::add_v(sz, req_vec(inst, dst)?, req_vec(inst, rn)?, m))
        }
        VecAluOp::Sub => {
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::sub_v(sz, req_vec(inst, dst)?, req_vec(inst, rn)?, m))
        }
        VecAluOp::Mul => {
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::mul_v(sz, req_vec(inst, dst)?, req_vec(inst, rn)?, m))
        }
        VecAluOp::Mla => {
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::mla_v(sz, req_vec(inst, dst)?, req_vec(inst, rn)?, m))
        }
        VecAluOp::Mls => {
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::mls_v(sz, req_vec(inst, dst)?, req_vec(inst, rn)?, m))
        }
        VecAluOp::Smin => {
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::minmax_v(false, true, sz, req_vec(inst, dst)?, req_vec(inst, rn)?, m))
        }
        VecAluOp::Smax => {
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::minmax_v(false, false, sz, req_vec(inst, dst)?, req_vec(inst, rn)?, m))
        }
        VecAluOp::Umin => {
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::minmax_v(true, true, sz, req_vec(inst, dst)?, req_vec(inst, rn)?, m))
        }
        VecAluOp::Umax => {
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::minmax_v(true, false, sz, req_vec(inst, dst)?, req_vec(inst, rn)?, m))
        }
        VecAluOp::Sqadd => {
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::sat_addsub_v(false, false, sz, req_vec(inst, dst)?, req_vec(inst, rn)?, m))
        }
        VecAluOp::Uqadd => {
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::sat_addsub_v(true, false, sz, req_vec(inst, dst)?, req_vec(inst, rn)?, m))
        }
        VecAluOp::Sqsub => {
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::sat_addsub_v(false, true, sz, req_vec(inst, dst)?, req_vec(inst, rn)?, m))
        }
        VecAluOp::Uqsub => {
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::sat_addsub_v(true, true, sz, req_vec(inst, dst)?, req_vec(inst, rn)?, m))
        }
        VecAluOp::Fadd => {
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::fadd_v(double, req_vec(inst, dst)?, req_vec(inst, rn)?, m))
        }
        VecAluOp::Fsub => {
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::fsub_v(double, req_vec(inst, dst)?, req_vec(inst, rn)?, m))
        }
        VecAluOp::Fmul => {
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::fmul_v(double, req_vec(inst, dst)?, req_vec(inst, rn)?, m))
        }
        VecAluOp::Fmla => {
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::fmla_v(double, req_vec(inst, dst)?, req_vec(inst, rn)?, m))
        }
        VecAluOp::Fmls => {
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::fmls_v(double, req_vec(inst, dst)?, req_vec(inst, rn)?, m))
        }
        VecAluOp::Cmeq => {
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::cmeq_v(true, sz, req_vec(inst, dst)?, req_vec(inst, rn)?, m))
        }
        VecAluOp::Cmgt => {
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::cmgt_v(false, sz, req_vec(inst, dst)?, req_vec(inst, rn)?, m))
        }
        VecAluOp::Cmge => {
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::cmge_v(false, sz, req_vec(inst, dst)?, req_vec(inst, rn)?, m))
        }
        VecAluOp::Cmhi => {
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::cmgt_v(true, sz, req_vec(inst, dst)?, req_vec(inst, rn)?, m))
        }
        VecAluOp::Cmhs => {
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::cmge_v(true, sz, req_vec(inst, dst)?, req_vec(inst, rn)?, m))
        }
        VecAluOp::DupGen => Ok(enc::dup_gen_v(sz, req_vec(inst, dst)?, req_gpr_or_zr(inst, rn)?)),
        VecAluOp::DupElem0 => Ok(enc::dup_elem0_v(sz, req_vec(inst, dst)?, req_vec(inst, rn)?)),
        VecAluOp::Addv => Ok(enc::addv_v(sz, req_vec(inst, dst)?, req_vec(inst, rn)?)),
        VecAluOp::AddpD => Ok(enc::addp_d(req_vec(inst, dst)?, req_vec(inst, rn)?)),
        VecAluOp::FaddpScalar => Ok(enc::faddp_scalar(double, req_vec(inst, dst)?, req_vec(inst, rn)?)),
        VecAluOp::FaddpV => {
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::faddp_v(double, req_vec(inst, dst)?, req_vec(inst, rn)?, m))
        }
        VecAluOp::UminvB => Ok(enc::uminv_b(req_vec(inst, dst)?, req_vec(inst, rn)?)),
        VecAluOp::UmovLane { lane } => {
            Ok(enc::umov_lane(sz, u8_to_u32(lane), req_gpr_or_zr(inst, dst)?, req_vec(inst, rn)?))
        }
        VecAluOp::Ext { imm4 } => {
            let m = req_vec(inst, rm.ok_or(MEncodeError::InvalidOperand { inst })?)?;
            Ok(enc::ext_v(req_vec(inst, dst)?, req_vec(inst, rn)?, m, u8_to_u32(imm4)))
        }
    }
}

fn encode_inst(
    op: &MOp,
    inst: MInst,
    word_idx: u32,
    is_relaxed: bool,
    block_word_off: &[u32],
    out: &mut Vec<u32, Alloc<'_>>,
) -> Result<(), MEncodeError> {
    match *op {
        MOp::Mov { dst, src, wide } => {
            let rd = req_gpr_or_zr(inst, dst)?;
            let rm = req_gpr_or_zr(inst, src)?;
            out.push(enc::sf(wide, enc::mov(rd, rm)));
        }
        MOp::MovImm { dst, imm, wide } => {
            let rd = req_gpr_or_zr(inst, dst)?;
            let (buf, count) = encode_mov_imm_words(rd, imm, wide);
            for &w in buf.get(..to_idx(count)).unwrap_or(&[]) {
                out.push(w);
            }
        }
        MOp::MovSp { dst, src } => {
            let rd = req_gpr_or_sp(inst, dst)?;
            let rn = req_gpr_or_sp(inst, src)?;
            out.push(enc::add_imm(rd, rn, 0, false));
        }
        MOp::AddImm { dst, rn, imm12, shift12, wide, sets_flags } => {
            let rd = if sets_flags { req_gpr_or_zr(inst, dst)? } else { req_gpr_or_sp(inst, dst)? };
            let n = req_gpr_or_sp(inst, rn)?;
            let base = enc::add_imm(rd, n, u16_to_u32(imm12), shift12);
            let word = if sets_flags { base | (1u32 << 29) } else { base };
            out.push(enc::sf(wide, word));
        }
        MOp::SubImm { dst, rn, imm12, shift12, wide, sets_flags } => {
            let rd = if sets_flags { req_gpr_or_zr(inst, dst)? } else { req_gpr_or_sp(inst, dst)? };
            let n = req_gpr_or_sp(inst, rn)?;
            let word = if sets_flags {
                enc::subs_imm(rd, n, u16_to_u32(imm12), shift12)
            } else {
                enc::sub_imm(rd, n, u16_to_u32(imm12), shift12)
            };
            out.push(enc::sf(wide, word));
        }
        MOp::AddReg { dst, rn, rm, shift, wide, sets_flags } => {
            let rd = req_gpr_or_zr(inst, dst)?;
            let n = req_gpr_or_zr(inst, rn)?;
            let m = req_gpr_or_zr(inst, rm)?;
            let base = if sets_flags { enc::adds(rd, n, m) } else { enc::add(rd, n, m) };
            out.push(enc::sf(wide, base | shift_bits(shift)));
        }
        MOp::SubReg { dst, rn, rm, shift, wide, sets_flags } => {
            let rd = req_gpr_or_zr(inst, dst)?;
            let n = req_gpr_or_zr(inst, rn)?;
            let m = req_gpr_or_zr(inst, rm)?;
            let base = if sets_flags { enc::subs(rd, n, m) } else { enc::sub(rd, n, m) };
            out.push(enc::sf(wide, base | shift_bits(shift)));
        }
        MOp::AddExt { dst, rn, rm, ext, wide, sets_flags } => {
            let rd = if sets_flags { req_gpr_or_zr(inst, dst)? } else { req_gpr_or_sp(inst, dst)? };
            let n = req_gpr_or_sp(inst, rn)?;
            let m = req_gpr_or_zr(inst, rm)?;
            let base = match ext {
                MExt::Uxtb => enc::add_uxtb(rd, n, m),
                MExt::Uxth => enc::add_uxth(rd, n, m),
                MExt::Uxtw => enc::add_uxtw(rd, n, m),
                MExt::Sxtw => enc::add_uxtb(rd, n, m) | (0b110u32 << 13),
            };
            let word = if sets_flags { base | (1u32 << 29) } else { base };
            out.push(enc::sf(wide, word));
        }
        MOp::SubExt { dst, rn, rm, ext, wide, sets_flags } => {
            let rd = if sets_flags { req_gpr_or_zr(inst, dst)? } else { req_gpr_or_sp(inst, dst)? };
            let n = req_gpr_or_sp(inst, rn)?;
            let m = req_gpr_or_zr(inst, rm)?;
            let base = match ext {
                MExt::Uxtb => enc::sub_uxtb(rd, n, m),
                MExt::Uxth => enc::sub_uxth(rd, n, m),
                MExt::Uxtw => enc::sub_uxtw(rd, n, m),
                MExt::Sxtw => enc::sub_uxtb(rd, n, m) | (0b110u32 << 13),
            };
            let word = if sets_flags { base | (1u32 << 29) } else { base };
            out.push(enc::sf(wide, word));
        }
        MOp::LogicalImm { dst, rn, imm, kind, wide } => {
            let rd = match kind {
                LogicalKind::Ands => req_gpr_or_zr(inst, dst)?,
                LogicalKind::And | LogicalKind::Orr | LogicalKind::Eor => req_gpr_or_sp(inst, dst)?,
            };
            let n = req_gpr_or_zr(inst, rn)?;
            let width = if wide { 64 } else { 32 };
            let nrs = trunc_u32(enc::logical_imm(imm, width).ok_or(MEncodeError::InvalidOperand { inst })?);
            let word = match kind {
                LogicalKind::And => enc::and_imm_nrs(rd, n, nrs),
                LogicalKind::Orr => enc::orr_imm_nrs(rd, n, nrs),
                LogicalKind::Eor => enc::eor_imm_nrs(rd, n, nrs),
                LogicalKind::Ands => enc::ands_imm(rd, n, nrs),
            };
            out.push(enc::sf(wide, word));
        }
        MOp::LogicalReg { dst, rn, rm, shift, invert_rm, kind, wide } => {
            let rd = req_gpr_or_zr(inst, dst)?;
            let n = req_gpr_or_zr(inst, rn)?;
            let m = req_gpr_or_zr(inst, rm)?;
            let base = match (kind, invert_rm) {
                (LogicalKind::And, false) => enc::and(rd, n, m),
                (LogicalKind::And, true) => enc::bic(rd, n, m),
                (LogicalKind::Orr, false) => enc::orr(rd, n, m),
                (LogicalKind::Orr, true) => enc::orn(rd, n, m),
                (LogicalKind::Eor, false) => enc::eor(rd, n, m),
                (LogicalKind::Eor, true) => enc::eon(rd, n, m),
                (LogicalKind::Ands, false) => enc::ands(rd, n, m),
                (LogicalKind::Ands, true) => enc::ands(rd, n, m) | (1u32 << 21),
            };
            out.push(enc::sf(wide, base | shift_bits(shift)));
        }
        MOp::Mvn { dst, rm, shift, wide } => {
            let rd = req_gpr_or_zr(inst, dst)?;
            let m = req_gpr_or_zr(inst, rm)?;
            let (sk, amt) = match shift {
                Some(sh) => (sh.kind_bits(), u8_to_u32(sh.amount())),
                None => (0, 0),
            };
            out.push(enc::mvn_shifted(wide, rd, m, sk, amt));
        }
        MOp::ShiftVar { dst, rn, rm, kind, wide } => {
            let rd = req_gpr_or_zr(inst, dst)?;
            let n = req_gpr_or_zr(inst, rn)?;
            let m = req_gpr_or_zr(inst, rm)?;
            let base = match kind {
                ShiftKind::Lsl => enc::lslv(rd, n, m),
                ShiftKind::Lsr => enc::lsrv(rd, n, m),
                ShiftKind::Asr => enc::asrv(rd, n, m),
                ShiftKind::Ror => enc::rorv(rd, n, m),
            };
            out.push(enc::sf(wide, base));
        }
        MOp::Ubfm { dst, rn, immr, imms, wide } => {
            let rd = req_gpr_or_zr(inst, dst)?;
            let n = req_gpr_or_zr(inst, rn)?;
            let word = if wide {
                enc::ubfm(rd, n, u8_to_u32(immr), u8_to_u32(imms))
            } else {
                enc::ubfm_w(rd, n, u8_to_u32(immr), u8_to_u32(imms))
            };
            out.push(word);
        }
        MOp::Sbfm { dst, rn, immr, imms, wide } => {
            let rd = req_gpr_or_zr(inst, dst)?;
            let n = req_gpr_or_zr(inst, rn)?;
            let word = if wide {
                enc::sbfm(rd, n, u8_to_u32(immr), u8_to_u32(imms))
            } else {
                enc::sbfm_w(rd, n, u8_to_u32(immr), u8_to_u32(imms))
            };
            out.push(word);
        }
        MOp::Bfxil { dst, rn, lsb, width, wide } => {
            let rd = req_gpr_or_zr(inst, dst)?;
            let n = req_gpr_or_zr(inst, rn)?;
            out.push(enc::bfxil(wide, rd, n, u8_to_u32(lsb), u8_to_u32(width)));
        }
        MOp::Extr { dst, rn, rm, lsb, wide } => {
            let rd = req_gpr_or_zr(inst, dst)?;
            let n = req_gpr_or_zr(inst, rn)?;
            let m = req_gpr_or_zr(inst, rm)?;
            let word = if wide {
                enc::extr(rd, n, m, u8_to_u32(lsb))
            } else {
                enc::extr_w(rd, n, m, u8_to_u32(lsb))
            };
            out.push(word);
        }
        MOp::Rbit { dst, rn, wide } => {
            out.push(enc::sf(wide, enc::rbit(req_gpr_or_zr(inst, dst)?, req_gpr_or_zr(inst, rn)?)));
        }
        MOp::Clz { dst, rn, wide } => {
            out.push(enc::sf(wide, enc::clz(req_gpr_or_zr(inst, dst)?, req_gpr_or_zr(inst, rn)?)));
        }
        MOp::Rev { dst, rn, wide } => {
            let rd = req_gpr_or_zr(inst, dst)?;
            let n = req_gpr_or_zr(inst, rn)?;
            out.push(if wide { enc::rev(rd, n) } else { enc::rev_w(rd, n) });
        }
        MOp::Mul { dst, rn, rm, wide } => {
            out.push(enc::sf(
                wide,
                enc::mul(req_gpr_or_zr(inst, dst)?, req_gpr_or_zr(inst, rn)?, req_gpr_or_zr(inst, rm)?),
            ));
        }
        MOp::Madd { dst, rn, rm, ra, wide } => {
            out.push(enc::sf(
                wide,
                enc::madd(
                    req_gpr_or_zr(inst, dst)?,
                    req_gpr_or_zr(inst, rn)?,
                    req_gpr_or_zr(inst, rm)?,
                    req_gpr_or_zr(inst, ra)?,
                ),
            ));
        }
        MOp::Msub { dst, rn, rm, ra, wide } => {
            out.push(enc::sf(
                wide,
                enc::msub(
                    req_gpr_or_zr(inst, dst)?,
                    req_gpr_or_zr(inst, rn)?,
                    req_gpr_or_zr(inst, rm)?,
                    req_gpr_or_zr(inst, ra)?,
                ),
            ));
        }
        MOp::Umulh { dst, rn, rm } => {
            out.push(enc::umulh(
                req_gpr_or_zr(inst, dst)?,
                req_gpr_or_zr(inst, rn)?,
                req_gpr_or_zr(inst, rm)?,
            ));
        }
        MOp::Smulh { dst, rn, rm } => {
            out.push(enc::smulh(
                req_gpr_or_zr(inst, dst)?,
                req_gpr_or_zr(inst, rn)?,
                req_gpr_or_zr(inst, rm)?,
            ));
        }
        MOp::CarryAddSub { dst, rn, rm, is_sub, sets_flags } => {
            let rd = req_gpr_or_zr(inst, dst)?;
            let r1 = req_gpr_or_zr(inst, rn)?;
            let r2 = req_gpr_or_zr(inst, rm)?;
            let word = match (is_sub, sets_flags) {
                (false, false) => enc::adc(rd, r1, r2),
                (false, true) => enc::adcs(rd, r1, r2),
                (true, false) => enc::sbc(rd, r1, r2),
                (true, true) => enc::sbcs(rd, r1, r2),
            };
            out.push(word);
        }
        MOp::Udiv { dst, rn, rm, wide } => {
            out.push(enc::sf(
                wide,
                enc::udiv(req_gpr_or_zr(inst, dst)?, req_gpr_or_zr(inst, rn)?, req_gpr_or_zr(inst, rm)?),
            ));
        }
        MOp::Sdiv { dst, rn, rm, wide } => {
            out.push(enc::sf(
                wide,
                enc::sdiv(req_gpr_or_zr(inst, dst)?, req_gpr_or_zr(inst, rn)?, req_gpr_or_zr(inst, rm)?),
            ));
        }
        MOp::Umin { dst, rn, rm, wide } => {
            out.push(enc::sf(
                wide,
                enc::cssc_minmax(true, true, req_gpr_or_zr(inst, dst)?, req_gpr_or_zr(inst, rn)?, req_gpr_or_zr(inst, rm)?),
            ));
        }
        MOp::Umax { dst, rn, rm, wide } => {
            out.push(enc::sf(
                wide,
                enc::cssc_minmax(true, false, req_gpr_or_zr(inst, dst)?, req_gpr_or_zr(inst, rn)?, req_gpr_or_zr(inst, rm)?),
            ));
        }
        MOp::Smin { dst, rn, rm, wide } => {
            out.push(enc::sf(
                wide,
                enc::cssc_minmax(false, true, req_gpr_or_zr(inst, dst)?, req_gpr_or_zr(inst, rn)?, req_gpr_or_zr(inst, rm)?),
            ));
        }
        MOp::Smax { dst, rn, rm, wide } => {
            out.push(enc::sf(
                wide,
                enc::cssc_minmax(false, false, req_gpr_or_zr(inst, dst)?, req_gpr_or_zr(inst, rn)?, req_gpr_or_zr(inst, rm)?),
            ));
        }
        MOp::CmpImm { rn, imm12, shift12, wide } => {
            let n = req_gpr_or_sp(inst, rn)?;
            out.push(enc::sf(wide, enc::subs_imm(enc::ZR, n, u16_to_u32(imm12), shift12)));
        }
        MOp::CmpReg { rn, rm, shift, wide } => {
            let n = req_gpr_or_zr(inst, rn)?;
            let m = req_gpr_or_zr(inst, rm)?;
            out.push(enc::sf(wide, enc::cmp(n, m) | shift_bits(shift)));
        }
        MOp::TstImm { rn, imm, wide } => {
            let n = req_gpr_or_zr(inst, rn)?;
            let width = if wide { 64 } else { 32 };
            let nrs = trunc_u32(enc::logical_imm(imm, width).ok_or(MEncodeError::InvalidOperand { inst })?);
            out.push(enc::sf(wide, enc::ands_imm(enc::ZR, n, nrs)));
        }
        MOp::TstReg { rn, rm, wide } => {
            out.push(enc::sf(wide, enc::ands(enc::ZR, req_gpr_or_zr(inst, rn)?, req_gpr_or_zr(inst, rm)?)));
        }
        MOp::CcmpImm { rn, imm5, nzcv, cond, wide } => {
            out.push(enc::sf(
                wide,
                enc::ccmp_imm(req_gpr_or_zr(inst, rn)?, u8_to_u32(imm5), u8_to_u32(nzcv), cond.to_enc()),
            ));
        }
        MOp::CcmpReg { rn, rm, nzcv, cond, wide } => {
            out.push(enc::sf(
                wide,
                enc::ccmp_reg(
                    req_gpr_or_zr(inst, rn)?,
                    req_gpr_or_zr(inst, rm)?,
                    u8_to_u32(nzcv),
                    cond.to_enc(),
                ),
            ));
        }
        MOp::Csel { dst, rn, rm, cond, wide } => {
            out.push(enc::sf(
                wide,
                enc::csel(req_gpr_or_zr(inst, dst)?, req_gpr_or_zr(inst, rn)?, req_gpr_or_zr(inst, rm)?, cond.to_enc()),
            ));
        }
        MOp::Csinc { dst, rn, rm, cond, wide } => {
            out.push(enc::sf(
                wide,
                enc::csinc(req_gpr_or_zr(inst, dst)?, req_gpr_or_zr(inst, rn)?, req_gpr_or_zr(inst, rm)?, cond.to_enc()),
            ));
        }
        MOp::Csinv { dst, rn, rm, cond, wide } => {
            out.push(enc::sf(
                wide,
                enc::csinv(req_gpr_or_zr(inst, dst)?, req_gpr_or_zr(inst, rn)?, req_gpr_or_zr(inst, rm)?, cond.to_enc()),
            ));
        }
        MOp::Csneg { dst, rn, rm, cond, wide } => {
            out.push(enc::sf(
                wide,
                enc::csneg(req_gpr_or_zr(inst, dst)?, req_gpr_or_zr(inst, rn)?, req_gpr_or_zr(inst, rm)?, cond.to_enc()),
            ));
        }
        MOp::Ldr { dst, addr, size, signed } => {
            let rt = if size.is_fp_or_vec() { req_vec(inst, dst)? } else { req_gpr_or_zr(inst, dst)? };
            out.push(encode_mem_single(inst, rt, addr, size, true, signed)?);
        }
        MOp::Str { src, addr, size } => {
            let rt = if size.is_fp_or_vec() { req_vec(inst, src)? } else { req_gpr_or_zr(inst, src)? };
            out.push(encode_mem_single(inst, rt, addr, size, false, false)?);
        }
        MOp::Ldp { dst0, dst1, base, off, size, post_idx, pre_idx } => {
            let (rt0, rt1) = if size.is_fp_or_vec() {
                (req_vec(inst, dst0)?, req_vec(inst, dst1)?)
            } else {
                (req_gpr_or_zr(inst, dst0)?, req_gpr_or_zr(inst, dst1)?)
            };
            out.push(encode_pair_mem(inst, rt0, rt1, base, off, size, post_idx, pre_idx, true)?);
        }
        MOp::Stp { src0, src1, base, off, size, post_idx, pre_idx } => {
            let (rt0, rt1) = if size.is_fp_or_vec() {
                (req_vec(inst, src0)?, req_vec(inst, src1)?)
            } else {
                (req_gpr_or_zr(inst, src0)?, req_gpr_or_zr(inst, src1)?)
            };
            out.push(encode_pair_mem(inst, rt0, rt1, base, off, size, post_idx, pre_idx, false)?);
        }
        MOp::FpAlu { op: fp_op, dst, rn, rm, ra, double } => {
            out.push(encode_fp_alu(inst, fp_op, dst, rn, rm, ra, double)?);
        }
        MOp::FpCmp { rn, rm, double } => {
            out.push(enc::fcmp(double, req_vec(inst, rn)?, req_vec(inst, rm)?));
        }
        MOp::Fcsel { dst, rn, rm, cond, double } => {
            out.push(enc::fcsel(
                double,
                req_vec(inst, dst)?,
                req_vec(inst, rn)?,
                req_vec(inst, rm)?,
                cond.to_enc(),
            ));
        }
        MOp::VecAlu { op: vec_op, dst, rn, rm, elem_size } => {
            out.push(encode_vec_alu(inst, vec_op, dst, rn, rm, elem_size)?);
        }
        MOp::Adrp { dst } => {
            out.push(enc::adrp(req_gpr_or_zr(inst, dst)?, 0));
        }
        MOp::AddLo12 { dst, rn } => {
            out.push(enc::add_imm(req_gpr_or_sp(inst, dst)?, req_gpr_or_sp(inst, rn)?, 0, false));
        }
        MOp::AdrTable { dst, .. } => {
            out.push(enc::adr(req_gpr_or_zr(inst, dst)?, 0));
        }
        MOp::Bl => {
            out.push(enc::bl(0));
        }
        MOp::Blr { target } => {
            out.push(enc::blr(req_gpr_or_zr(inst, target)?));
        }
        MOp::Br { target } => {
            out.push(enc::br(req_gpr_or_zr(inst, target)?));
        }
        MOp::B { target } => {
            let delta = branch_delta_i32(inst, target, word_idx, block_word_off, IMM26_MIN_WORDS, IMM26_MAX_WORDS)?;
            out.push(enc::b(delta));
        }
        MOp::BCond { cond, target } => {
            if matches!(cond, MCond::Al | MCond::Nv) {
                let delta = branch_delta_i32(inst, target, word_idx, block_word_off, IMM26_MIN_WORDS, IMM26_MAX_WORDS)?;
                out.push(enc::b(delta));
                if is_relaxed {
                    out.push(enc::nop());
                }
            } else if is_relaxed {
                let b_word = word_idx.checked_add(1).ok_or(MEncodeError::CodeTooLarge)?;
                let delta = branch_delta_i32(inst, target, b_word, block_word_off, IMM26_MIN_WORDS, IMM26_MAX_WORDS)?;
                out.push(enc::b_cond(cond.invert().to_enc(), 2));
                out.push(enc::b(delta));
            } else {
                let delta = branch_delta_i32(inst, target, word_idx, block_word_off, IMM19_MIN_WORDS, IMM19_MAX_WORDS)?;
                out.push(enc::b_cond(cond.to_enc(), delta));
            }
        }
        MOp::Cbz { rt, target, wide } => {
            let r = req_gpr_or_zr(inst, rt)?;
            if is_relaxed {
                let b_word = word_idx.checked_add(1).ok_or(MEncodeError::CodeTooLarge)?;
                let delta = branch_delta_i32(inst, target, b_word, block_word_off, IMM26_MIN_WORDS, IMM26_MAX_WORDS)?;
                out.push(enc::sf(wide, enc::cbnz(r, 2)));
                out.push(enc::b(delta));
            } else {
                let delta = branch_delta_i32(inst, target, word_idx, block_word_off, IMM19_MIN_WORDS, IMM19_MAX_WORDS)?;
                out.push(enc::sf(wide, enc::cbz(r, delta)));
            }
        }
        MOp::Cbnz { rt, target, wide } => {
            let r = req_gpr_or_zr(inst, rt)?;
            if is_relaxed {
                let b_word = word_idx.checked_add(1).ok_or(MEncodeError::CodeTooLarge)?;
                let delta = branch_delta_i32(inst, target, b_word, block_word_off, IMM26_MIN_WORDS, IMM26_MAX_WORDS)?;
                out.push(enc::sf(wide, enc::cbz(r, 2)));
                out.push(enc::b(delta));
            } else {
                let delta = branch_delta_i32(inst, target, word_idx, block_word_off, IMM19_MIN_WORDS, IMM19_MAX_WORDS)?;
                out.push(enc::sf(wide, enc::cbnz(r, delta)));
            }
        }
        MOp::Tbz { rt, bit, target } => {
            let r = req_gpr_or_zr(inst, rt)?;
            let b = u8_to_u32(bit);
            if is_relaxed {
                let b_word = word_idx.checked_add(1).ok_or(MEncodeError::CodeTooLarge)?;
                let delta = branch_delta_i32(inst, target, b_word, block_word_off, IMM26_MIN_WORDS, IMM26_MAX_WORDS)?;
                out.push(enc::tbnz(r, b, 2));
                out.push(enc::b(delta));
            } else {
                let delta = branch_delta_i32(inst, target, word_idx, block_word_off, IMM14_MIN_WORDS, IMM14_MAX_WORDS)?;
                out.push(enc::tbz(r, b, delta));
            }
        }
        MOp::Tbnz { rt, bit, target } => {
            let r = req_gpr_or_zr(inst, rt)?;
            let b = u8_to_u32(bit);
            if is_relaxed {
                let b_word = word_idx.checked_add(1).ok_or(MEncodeError::CodeTooLarge)?;
                let delta = branch_delta_i32(inst, target, b_word, block_word_off, IMM26_MIN_WORDS, IMM26_MAX_WORDS)?;
                out.push(enc::tbz(r, b, 2));
                out.push(enc::b(delta));
            } else {
                let delta = branch_delta_i32(inst, target, word_idx, block_word_off, IMM14_MIN_WORDS, IMM14_MAX_WORDS)?;
                out.push(enc::tbnz(r, b, delta));
            }
        }
        MOp::Ret => out.push(enc::ret()),
        MOp::Brk { imm } => out.push(enc::brk(u16_to_u32(imm))),
        MOp::Prfm { base, imm12 } => {
            let b = req_gpr_or_sp(inst, base)?;
            out.push(enc::prfm_pldl1keep(b, u16_to_u32(imm12)));
        }
        MOp::Udf => out.push(0),
        MOp::Nop => out.push(enc::nop()),
        MOp::ParallelCopy { .. } => return Err(MEncodeError::UnloweredParallelCopy { inst }),
        MOp::RawWord { word, .. } => out.push(word),
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// Unit & Apple Silicon Hardware Execution Tests (NO `Global` / NO `std::`)
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use blaze_own::Alloc;
    use blaze_tier::{Os, RelocKind};

    use super::{
        AddrMode, DEFAULT_RET_LIVE_GPRS, FpAluOp, MBlock, MCond, MEncodeError, MFunc, MFuncOpts,
        MInst, MInstFlags, MNoteKind, MOp, MReg, MVerifyError, MemSize, OperandRole, PReg,
        RelocAt, TableAdrAt, TrapAt, UnwindAt, VReg, VRegClass, mencode, mverify,
    };
    use crate::enc::{self, X0, X9};

    fn test_alloc() -> Alloc<'static> {
        blaze_own::testing::heap().alloc()
    }

    fn new_post_ra(os: Os) -> MFunc<'static> {
        MFunc::new(MFuncOpts {
            os,
            is_ssa: false,
            ret_live_gprs: DEFAULT_RET_LIVE_GPRS,
            alloc: test_alloc(),
        })
    }

    fn new_ssa(os: Os) -> MFunc<'static> {
        MFunc::new(MFuncOpts {
            os,
            is_ssa: true,
            ret_live_gprs: DEFAULT_RET_LIVE_GPRS,
            alloc: test_alloc(),
        })
    }

    #[test]
    fn mverify_accepts_valid_ssa_and_post_ra_functions() {
        // 1. Valid SSA function with VRegs, ParallelCopy, and chained `ccmp` VFlags.
        let mut ssa = new_ssa(Os::MacOs);
        let v0 = ssa.alloc_vreg(VRegClass::Gpr64);
        let v1 = ssa.alloc_vreg(VRegClass::Gpr64);
        let vf0 = ssa.alloc_vreg(VRegClass::Fp64);
        let b0 = ssa.new_block(false);
        let b1 = ssa.new_block(true);

        ssa.begin_block(b0);
        ssa.push_inst(
            MOp::MovImm { dst: MReg::V(v0), imm: 10, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        ssa.push_inst(
            MOp::FpAlu {
                op: FpAluOp::MovFromGpr,
                dst: MReg::V(vf0),
                rn: MReg::V(v0),
                rm: None,
                ra: None,
                double: true,
            },
            MInstFlags::NONE,
        );
        let c0 = ssa.push_inst(
            MOp::CmpImm { rn: MReg::V(v0), imm12: 5, shift12: false, wide: true },
            MInstFlags::NONE,
        );
        ssa.set_vflag_def(c0, 0);
        let c1 = ssa.push_inst(
            MOp::CcmpImm { rn: MReg::V(v0), imm5: 10, nzcv: 0, cond: MCond::Ne, wide: true },
            MInstFlags::NONE,
        );
        ssa.set_vflag_use(c1, 0);
        ssa.set_vflag_def(c1, 1);
        let sel = ssa.push_inst(
            MOp::Csel { dst: MReg::V(v1), rn: MReg::V(v0), rm: MReg::XZR, cond: MCond::Eq, wide: true },
            MInstFlags::NONE,
        );
        ssa.set_vflag_use(sel, 1);
        ssa.push_parallel_copy(&[(MReg::p(PReg::X0), MReg::V(v1))], MInstFlags::NONE);
        ssa.push_inst(MOp::B { target: b1 }, MInstFlags::TERMINATOR);
        ssa.end_block(b0);

        ssa.begin_block(b1);
        ssa.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        ssa.end_block(b1);

        ssa.rebuild_cfg(test_alloc());
        assert_eq!(mverify(&ssa), Ok(()));
        assert_eq!(ssa.block_succs(b0), &[b1]);
        assert_eq!(ssa.block_preds(b1), &[b0]);

        // 2. Valid post-RA function using `SP` legally and `XZR` legally.
        let mut post = new_post_ra(Os::MacOs);
        let pb0 = post.new_block(false);
        post.begin_block(pb0);
        post.push_inst(
            MOp::SubImm { dst: MReg::SP, rn: MReg::SP, imm12: 16, shift12: false, wide: true, sets_flags: false },
            MInstFlags::NONE,
        );
        post.push_inst(
            MOp::MovSp { dst: MReg::p(PReg::X1), src: MReg::SP },
            MInstFlags::NONE,
        );
        post.push_inst(
            MOp::Stp {
                src0: MReg::p(PReg::X0),
                src1: MReg::XZR,
                base: MReg::SP,
                off: 0,
                size: MemSize::X64,
                post_idx: false,
                pre_idx: false,
            },
            MInstFlags::MEM_WRITE,
        );
        post.push_inst(
            MOp::AddImm { dst: MReg::SP, rn: MReg::SP, imm12: 16, shift12: false, wide: true, sets_flags: false },
            MInstFlags::NONE,
        );
        post.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        post.end_block(pb0);
        assert_eq!(mverify(&post), Ok(()));
    }

    #[test]
    fn mverify_rejects_x18_on_macos_but_allows_on_linux() {
        let mut mac = new_post_ra(Os::MacOs);
        let b0 = mac.new_block(false);
        mac.begin_block(b0);
        let bad = mac.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X18), src: MReg::p(PReg::X0), wide: true },
            MInstFlags::NONE,
        );
        mac.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        mac.end_block(b0);
        assert_eq!(
            mverify(&mac),
            Err(MVerifyError::ReservedPlatformReg { inst: bad, preg: PReg::X18 })
        );

        let mut linux = new_post_ra(Os::Linux);
        let lb0 = linux.new_block(false);
        linux.begin_block(lb0);
        linux.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X18), src: MReg::p(PReg::X0), wide: true },
            MInstFlags::NONE,
        );
        linux.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        linux.end_block(lb0);
        assert_eq!(mverify(&linux), Ok(()));
    }

    #[test]
    fn mverify_rejects_sp_and_xzr_misuse() {
        // 1. `SP` in `Mov` (must use `MovSp`!).
        let mut f1 = new_post_ra(Os::MacOs);
        let b0 = f1.new_block(false);
        f1.begin_block(b0);
        let bad1 = f1.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::SP, wide: true },
            MInstFlags::NONE,
        );
        f1.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        f1.end_block(b0);
        assert_eq!(
            mverify(&f1),
            Err(MVerifyError::IllegalPRegForOperand {
                inst: bad1,
                preg: PReg::Sp,
                expected: OperandRole::GprOrZr,
            })
        );

        // 2. `SP` as destination of flag-setting `AddImm` (`ADDS` reg 31 is `XZR`, never `SP`!).
        let mut f2 = new_post_ra(Os::MacOs);
        let b0 = f2.new_block(false);
        f2.begin_block(b0);
        let bad2 = f2.push_inst(
            MOp::AddImm { dst: MReg::SP, rn: MReg::SP, imm12: 8, shift12: false, wide: true, sets_flags: true },
            MInstFlags::NONE,
        );
        f2.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        f2.end_block(b0);
        assert_eq!(
            mverify(&f2),
            Err(MVerifyError::IllegalPRegForOperand {
                inst: bad2,
                preg: PReg::Sp,
                expected: OperandRole::GprOrZr,
            })
        );

        // 3. `XZR` as memory base in `Ldr` (hardware reg 31 in memory base is `SP`, never `XZR`!).
        let mut f3 = new_post_ra(Os::MacOs);
        let b0 = f3.new_block(false);
        f3.begin_block(b0);
        let bad3 = f3.push_inst(
            MOp::Ldr {
                dst: MReg::p(PReg::X0),
                addr: AddrMode::BaseOff { base: MReg::XZR, off: 0 },
                size: MemSize::X64,
                signed: false,
            },
            MInstFlags::MEM_READ,
        );
        f3.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        f3.end_block(b0);
        assert_eq!(
            mverify(&f3),
            Err(MVerifyError::IllegalPRegForOperand {
                inst: bad3,
                preg: PReg::Xzr,
                expected: OperandRole::GprOrSp,
            })
        );
    }

    #[test]
    fn mverify_rejects_overlapping_live_vflags_and_unlowered_ssa_post_ra() {
        // 1. Overlapping two live VFlags: `cmp` defines 0, second `cmp` defines 1, `csel` uses 0!
        let mut f = new_post_ra(Os::MacOs);
        let b0 = f.new_block(false);
        f.begin_block(b0);
        let c0 = f.push_inst(
            MOp::CmpImm { rn: MReg::p(PReg::X0), imm12: 0, shift12: false, wide: true },
            MInstFlags::NONE,
        );
        f.set_vflag_def(c0, 0);
        let c1 = f.push_inst(
            MOp::CmpImm { rn: MReg::p(PReg::X1), imm12: 1, shift12: false, wide: true },
            MInstFlags::NONE,
        );
        f.set_vflag_def(c1, 1);
        let sel = f.push_inst(
            MOp::Csel {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X0),
                rm: MReg::p(PReg::X1),
                cond: MCond::Eq,
                wide: true,
            },
            MInstFlags::NONE,
        );
        f.set_vflag_use(sel, 0);
        f.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        f.end_block(b0);
        assert_eq!(
            mverify(&f),
            Err(MVerifyError::OverlappingLiveVFlags {
                block: b0,
                inst: sel,
                expected: 0,
                active: Some(1),
            })
        );

        // 2. Unlowered `VReg` when `is_ssa == false`.
        let mut f_vreg = new_post_ra(Os::MacOs);
        let v0 = f_vreg.alloc_vreg(VRegClass::Gpr64);
        let b0 = f_vreg.new_block(false);
        f_vreg.begin_block(b0);
        let bad_v = f_vreg.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::V(v0), wide: true },
            MInstFlags::NONE,
        );
        f_vreg.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        f_vreg.end_block(b0);
        assert_eq!(
            mverify(&f_vreg),
            Err(MVerifyError::UnloweredVReg { inst: bad_v, vreg: VReg(0) })
        );

        // 3. Unlowered `ParallelCopy` when `is_ssa == false`.
        let mut f_pc = new_post_ra(Os::MacOs);
        let b0 = f_pc.new_block(false);
        f_pc.begin_block(b0);
        let bad_pc = f_pc.push_parallel_copy(&[(MReg::p(PReg::X0), MReg::p(PReg::X1))], MInstFlags::NONE);
        f_pc.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        f_pc.end_block(b0);
        assert_eq!(
            mverify(&f_pc),
            Err(MVerifyError::UnloweredParallelCopy { inst: bad_pc })
        );
    }

    #[test]
    fn mencode_records_exact_note_offsets_across_multiword_movimm_and_align16() {
        let alloc = test_alloc();
        let mut func = new_post_ra(Os::MacOs);
        let b0 = func.new_block(false);
        let b1 = func.new_block(true); // forces 16-byte alignment padding!

        func.begin_block(b0);
        // 4 words (byte offsets 0..16): 0x1234_5678_9abc_def0
        func.push_inst(
            MOp::MovImm { dst: MReg::p(PReg::X0), imm: 0x1234_5678_9abc_def0, wide: true },
            MInstFlags::REMATERIALIZABLE,
        );
        // Word 4 (byte 16): adrp x9 + Reloc(Page21)
        let i_adrp = func.push_inst(MOp::Adrp { dst: MReg::p(PReg::X9) }, MInstFlags::PINNED);
        func.attach_note(
            i_adrp,
            MNoteKind::Reloc { kind: RelocKind::Page21, target: 7, addend: 0 },
        );
        // Word 5 (byte 20): add x9, x9, #0 + Reloc(PageOff12)
        let i_lo12 = func.push_inst(
            MOp::AddLo12 { dst: MReg::p(PReg::X9), rn: MReg::p(PReg::X9) },
            MInstFlags::PINNED,
        );
        func.attach_note(
            i_lo12,
            MNoteKind::Reloc { kind: RelocKind::PageOff12 { scale: 0 }, target: 7, addend: 0 },
        );
        // Word 6 (byte 24): bl + chained Reloc(Branch26) AND Unwind(3)
        let i_bl = func.push_inst(MOp::Bl, MInstFlags::CALL);
        func.attach_note(
            i_bl,
            MNoteKind::Reloc { kind: RelocKind::Branch26, target: 11, addend: 0 },
        );
        func.attach_note(i_bl, MNoteKind::Unwind { unwind_idx: 3 });
        // Word 7 (byte 28): adr_table x9, table_id=2
        func.push_inst(MOp::AdrTable { dst: MReg::p(PReg::X9), table_id: 2 }, MInstFlags::PINNED);
        // Word 8 (byte 32): b b1 (ends at word 9, so b1 pads 3 NOPs to align to word 12 = byte 48)
        func.push_inst(MOp::B { target: b1 }, MInstFlags::TERMINATOR);
        func.end_block(b0);

        func.begin_block(b1);
        // Word 12 (byte 48): brk #0xff00 + Trap(5)
        let i_brk = func.push_inst(MOp::Brk { imm: 0xff00 }, MInstFlags::TERMINATOR.union(MInstFlags::MAY_TRAP));
        func.attach_note(i_brk, MNoteKind::Trap { trap_idx: 5 });
        func.end_block(b1);

        let enc_out = mencode(&func, alloc).expect("mencode must succeed");
        assert_eq!(enc_out.relaxed_branches, 0);
        assert_eq!(enc_out.block_word_off.as_slice(), &[0, 12]);
        assert_eq!(enc_out.words.len(), 13);

        // Verify alignment NOPs at words 9, 10, 11.
        assert_eq!(enc_out.words[9], enc::nop());
        assert_eq!(enc_out.words[10], enc::nop());
        assert_eq!(enc_out.words[11], enc::nop());

        // Verify exact instruction words via `crate::enc::*`.
        assert_eq!(enc_out.words[0], enc::movz(X0, 0xdef0, 0));
        assert_eq!(enc_out.words[1], enc::movk(X0, 0x9abc, 1));
        assert_eq!(enc_out.words[2], enc::movk(X0, 0x5678, 2));
        assert_eq!(enc_out.words[3], enc::movk(X0, 0x1234, 3));
        assert_eq!(enc_out.words[4], enc::adrp(X9, 0));
        assert_eq!(enc_out.words[5], enc::add_imm(X9, X9, 0, false));
        assert_eq!(enc_out.words[6], enc::bl(0));
        assert_eq!(enc_out.words[7], enc::adr(X9, 0));
        assert_eq!(enc_out.words[8], enc::b(4)); // word 8 -> word 12 (+4 words)
        assert_eq!(enc_out.words[12], enc::brk(0xff00));

        // Verify exact metadata offsets with ZERO manual remapping!
        assert_eq!(
            enc_out.reloc_byte_offs.as_slice(),
            &[
                RelocAt { byte_off: 16, kind: RelocKind::Page21, target: 7, addend: 0 },
                RelocAt { byte_off: 20, kind: RelocKind::PageOff12 { scale: 0 }, target: 7, addend: 0 },
                RelocAt { byte_off: 24, kind: RelocKind::Branch26, target: 11, addend: 0 },
            ]
        );
        assert_eq!(enc_out.unwind_byte_offs.as_slice(), &[UnwindAt { byte_off: 24, unwind_idx: 3 }]);
        assert_eq!(enc_out.trap_byte_offs.as_slice(), &[TrapAt { byte_off: 48, trap_idx: 5 }]);
        assert_eq!(enc_out.table_adr_word_idxs.as_slice(), &[TableAdrAt { table_id: 2, word_idx: 7 }]);

        // Verify SSA function is rejected by `mencode`.
        let ssa = new_ssa(Os::MacOs);
        assert_eq!(mencode(&ssa, alloc).err(), Some(MEncodeError::StillInSsa));
    }

    #[test]
    fn mencode_relaxes_tbz_beyond_32kb_range() {
        let alloc = test_alloc();
        let mut func = new_post_ra(Os::MacOs);
        let b0 = func.new_block(false);
        let b_sled = func.new_block(false);
        let b_far = func.new_block(false);

        // Block 0: `tbz x0, #0, b_far` -> target is > 9000 words (> 36 KiB > ±32 KiB Imm14 limit!).
        func.begin_block(b0);
        func.push_inst(MOp::Tbz { rt: MReg::p(PReg::X0), bit: 0, target: b_far }, MInstFlags::TERMINATOR);
        func.end_block(b0);

        // Block 1: 9000 NOPs + `mov x0, #42; ret`
        func.begin_block(b_sled);
        for _ in 0..9000 {
            func.push_inst(MOp::Nop, MInstFlags::NONE);
        }
        func.push_inst(MOp::MovImm { dst: MReg::p(PReg::X0), imm: 42, wide: true }, MInstFlags::NONE);
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_sled);

        // Block 2 (far target): `mov x0, #99; ret`
        func.begin_block(b_far);
        func.push_inst(MOp::MovImm { dst: MReg::p(PReg::X0), imm: 99, wide: true }, MInstFlags::NONE);
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_far);

        let enc_out = mencode(&func, alloc).expect("mencode must relax out-of-range tbz");
        assert_eq!(enc_out.relaxed_branches, 1);
        // Word 0: inverted `tbnz x0, #0, +2` (skips the `b` at word 1 to land on `b_sled` at word 2).
        assert_eq!(enc_out.words[0], enc::tbnz(X0, 0, 2));
        // `b_sled` starts at word 2 (9002 words long -> `b_far` starts at word 9004).
        assert_eq!(enc_out.block_word_off[1], 2);
        assert_eq!(enc_out.block_word_off[2], 9004);
        // Word 1: unconditional `b +(9004 - 1)` = `b +9003`!
        assert_eq!(enc_out.words[1], enc::b(9003));

        #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
        {
            let mut bytes: blaze_own::Vec<u8, Alloc<'static>> =
                blaze_own::Vec::with_capacity_in(enc_out.words.len() * 4, alloc);
            for &w in enc_out.words.iter() {
                for b in w.to_le_bytes() {
                    bytes.push(b);
                }
            }
            let exec = blaze_inproc::exec::Executable::new(&bytes, 0).expect("W^X exec mapping must succeed");
            // SAFETY: Valid AAPCS64 leaf function touching only caller-saved `X0`.
            let f: extern "C" fn(u64) -> u64 = unsafe { exec.as_fn() };
            assert_eq!(f(0), 99, "even input (bit 0 == 0) must take relaxed >32KB branch to b_far");
            assert_eq!(f(2), 99, "even input (bit 0 == 0) must take relaxed >32KB branch to b_far");
            assert_eq!(f(1), 42, "odd input (bit 0 == 1) must skip relaxed branch into b_sled");
            assert_eq!(f(7), 42, "odd input (bit 0 == 1) must skip relaxed branch into b_sled");
        }
    }

    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    #[test]
    fn mfunc_loop_verifies_encodes_and_executes_on_apple_silicon() {
        use blaze_own::Vec;

        use super::{LogicalKind, MShift};
        use crate::enc::X1;

        // Compute `f(n) = sum_{i=1..=n} (i * 3 + (i & 1) + (i << 1))` on real Apple Silicon hardware.
        let alloc = test_alloc();
        let mut func = new_post_ra(Os::MacOs);
        let b_entry = func.new_block(false);
        let b_loop = func.new_block(true);
        let b_exit = func.new_block(false);

        // Entry: x1 = acc (0), x2 = i (1), x3 = 3; if x0 == 0 -> b_exit
        func.begin_block(b_entry);
        func.push_inst(MOp::MovImm { dst: MReg::p(PReg::X1), imm: 0, wide: true }, MInstFlags::NONE);
        func.push_inst(MOp::MovImm { dst: MReg::p(PReg::X2), imm: 1, wide: true }, MInstFlags::NONE);
        func.push_inst(MOp::MovImm { dst: MReg::p(PReg::X3), imm: 3, wide: true }, MInstFlags::NONE);
        func.push_inst(
            MOp::Cbz { rt: MReg::p(PReg::X0), target: b_exit, wide: true },
            MInstFlags::TERMINATOR,
        );
        func.end_block(b_entry);

        // Loop (16B-aligned):
        //   and x4, x2, #1
        //   add x1, x1, x4
        //   add x1, x1, x2, lsl #1
        //   madd x1, x2, x3, x1
        //   add x2, x2, #1
        //   cmp x2, x0        (vflag_def = 0)
        //   b.ls b_loop       (vflag_use = 0)
        func.begin_block(b_loop);
        func.push_inst(
            MOp::LogicalImm {
                dst: MReg::p(PReg::X4),
                rn: MReg::p(PReg::X2),
                imm: 1,
                kind: LogicalKind::And,
                wide: true,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X1),
                rn: MReg::p(PReg::X1),
                rm: MReg::p(PReg::X4),
                shift: None,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddReg {
                dst: MReg::p(PReg::X1),
                rn: MReg::p(PReg::X1),
                rm: MReg::p(PReg::X2),
                shift: Some(MShift::Lsl(1)),
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::Madd {
                dst: MReg::p(PReg::X1),
                rn: MReg::p(PReg::X2),
                rm: MReg::p(PReg::X3),
                ra: MReg::p(PReg::X1),
                wide: true,
            },
            MInstFlags::NONE,
        );
        func.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X2),
                rn: MReg::p(PReg::X2),
                imm12: 1,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        let cmp_inst = func.push_inst(
            MOp::CmpReg { rn: MReg::p(PReg::X2), rm: MReg::p(PReg::X0), shift: None, wide: true },
            MInstFlags::NONE,
        );
        func.set_vflag_def(cmp_inst, 0);
        let br_inst = func.push_inst(
            MOp::BCond { cond: MCond::Ls, target: b_loop },
            MInstFlags::TERMINATOR,
        );
        func.set_vflag_use(br_inst, 0);
        func.end_block(b_loop);

        // Exit: `mov x0, x1; ret`
        func.begin_block(b_exit);
        func.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X0), src: MReg::p(PReg::X1), wide: true },
            MInstFlags::NONE,
        );
        func.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        func.end_block(b_exit);

        func.rebuild_cfg(alloc);
        assert_eq!(mverify(&func), Ok(()));
        assert_eq!(func.block_succs(b_entry), &[b_exit, b_loop]);
        assert_eq!(func.block_succs(b_loop), &[b_loop, b_exit]);

        let enc_out = mencode(&func, alloc).expect("mencode must succeed");
        assert_eq!(enc_out.block_word_off[1] % 4, 0, "loop block must be 16-byte aligned");
        assert_eq!(enc_out.words[0], enc::movz(X1, 0, 0));

        let mut bytes: Vec<u8, Alloc<'static>> = Vec::with_capacity_in(enc_out.words.len() * 4, alloc);
        for &w in enc_out.words.iter() {
            for b in w.to_le_bytes() {
                bytes.push(b);
            }
        }
        let exec = blaze_inproc::exec::Executable::new(&bytes, 0).expect("W^X exec mapping must succeed");
        // SAFETY: Valid AAPCS64 frameless leaf touching only caller-saved `X0..=X4`.
        let f: extern "C" fn(u64) -> u64 = unsafe { exec.as_fn() };

        for &n in &[0u64, 1, 2, 5, 10, 100, 1000] {
            let mut expected: u64 = 0;
            for i in 1..=n {
                expected = expected.wrapping_add(i.wrapping_mul(3) + (i & 1) + (i << 1));
            }
            assert_eq!(f(n), expected, "hardware execution mismatch for n={n}");
        }
    }

    #[test]
    fn mencode_align16_pads_after_uncond_term_but_skips_fallthrough() {
        let alloc = test_alloc();

        // Case 4a (`block_align16 = true`, no backward branch, after unconditional terminator):
        // Unconditional-`B` predecessor (even across an intervening empty block) pads `align16`
        // target cleanly to a 16-byte boundary (zero executed `NOP`s).
        let mut uncond = new_post_ra(Os::MacOs);
        let u_b0 = uncond.new_block(false);
        let u_empty = uncond.new_block(false);
        let u_b2 = uncond.new_block(true);

        uncond.begin_block(u_b0);
        uncond.push_inst(MOp::MovImm { dst: MReg::p(PReg::X0), imm: 1, wide: true }, MInstFlags::NONE);
        uncond.push_inst(MOp::B { target: u_b2 }, MInstFlags::TERMINATOR);
        uncond.end_block(u_b0);

        uncond.begin_block(u_empty);
        uncond.end_block(u_empty);

        uncond.begin_block(u_b2);
        uncond.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        uncond.end_block(u_b2);

        let u_enc = mencode(&uncond, alloc).expect("mencode uncond align16 must succeed");
        assert_eq!(u_enc.block_word_off.as_slice(), &[0, 2, 4]);
        assert_eq!(u_enc.words.len(), 5);
        assert_eq!(u_enc.words[1], enc::b(3));
        assert_eq!(u_enc.words[2], enc::nop());
        assert_eq!(u_enc.words[3], enc::nop());
        assert_eq!(u_enc.words[4], enc::ret());

        // Case 1 (`block_align16 = true` + backward branch): true RPO loop header reached by
        // fallthrough from pre-header `f_prep` (even across an intervening empty block) IS ALWAYS
        // padded to 16-byte alignment (`mod 4 == 0`).
        let mut fall = new_post_ra(Os::MacOs);
        let f_entry = fall.new_block(false);
        let f_prep = fall.new_block(false);
        let f_empty = fall.new_block(false);
        let f_loop = fall.new_block(true);
        let f_exit = fall.new_block(false);

        fall.begin_block(f_entry);
        let f_sub = fall.push_inst(
            MOp::SubImm {
                dst: MReg::p(PReg::X15),
                rn: MReg::p(PReg::X15),
                imm12: 1,
                shift12: false,
                wide: true,
                sets_flags: true,
            },
            MInstFlags::NONE,
        );
        fall.set_vflag_def(f_sub, 0);
        let f_beq = fall.push_inst(
            MOp::BCond { cond: MCond::Eq, target: f_exit },
            MInstFlags::TERMINATOR,
        );
        fall.set_vflag_use(f_beq, 0);
        fall.end_block(f_entry);

        fall.begin_block(f_prep);
        fall.push_inst(
            MOp::Mov { dst: MReg::p(PReg::X25), src: MReg::p(PReg::X26), wide: true },
            MInstFlags::NONE,
        );
        fall.end_block(f_prep);

        fall.begin_block(f_empty);
        fall.end_block(f_empty);

        fall.begin_block(f_loop);
        let f_cmp = fall.push_inst(
            MOp::CmpReg {
                rn: MReg::p(PReg::X8),
                rm: MReg::p(PReg::X9),
                shift: None,
                wide: true,
            },
            MInstFlags::NONE,
        );
        fall.set_vflag_def(f_cmp, 0);
        let f_blo = fall.push_inst(
            MOp::BCond { cond: MCond::Lo, target: f_loop },
            MInstFlags::TERMINATOR,
        );
        fall.set_vflag_use(f_blo, 0);
        fall.end_block(f_loop);

        fall.begin_block(f_exit);
        fall.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        fall.end_block(f_exit);

        let f_enc = mencode(&fall, alloc).expect("mencode backedge align16 must succeed");
        assert_eq!(f_enc.block_word_off.as_slice(), &[0, 2, 3, 4, 6]);
        assert_eq!(f_enc.words.len(), 7);
        assert_eq!(f_enc.words[3], enc::nop());
        assert_eq!(f_enc.words[1], enc::b_cond(enc::Cond::Eq, 5));
        assert_eq!(f_enc.words[5], enc::b_cond(enc::Cond::Lo, -1));
        assert_eq!(f_enc.words[6], enc::ret());

        // Case 4b (`block_align16 = true`, no backward branch, on a fallthrough path): branch
        // targets `f_entry` instead of `f_loop` -> emits ZERO fallthrough `NOP`s!
        fall.ops[super::to_idx(f_blo.0)] = MOp::BCond { cond: MCond::Lo, target: f_entry };
        let f_no_back = mencode(&fall, alloc).expect("mencode non-backedge fallthrough must succeed");
        assert_eq!(f_no_back.block_word_off.as_slice(), &[0, 2, 3, 3, 5]);
        assert_eq!(f_no_back.words.len(), 6);
        assert!(
            !f_no_back.words.iter().any(|&w| w == enc::nop()),
            "fallthrough-only non-backedge align16 block must emit ZERO alignment NOPs"
        );

        // Case 2 (`block_align16 = false` + backward branch from a cold tail on a fallthrough
        // path): non-loop diamond merge `d_merge` falls through from `d_hot` and is also targeted
        // backward by cold tail `d_cold` -> ZERO fallthrough `NOP`s!
        let mut diamond = new_post_ra(Os::MacOs);
        let d_entry = diamond.new_block(false);
        let d_hot = diamond.new_block(false);
        let d_merge = diamond.new_block(false);
        let d_cold = diamond.new_block(false);

        diamond.begin_block(d_entry);
        diamond.push_inst(
            MOp::Cbz { rt: MReg::p(PReg::X0), target: d_cold, wide: true },
            MInstFlags::TERMINATOR,
        );
        diamond.end_block(d_entry);

        diamond.begin_block(d_hot);
        diamond.push_inst(MOp::MovImm { dst: MReg::p(PReg::X0), imm: 1, wide: true }, MInstFlags::NONE);
        diamond.end_block(d_hot);

        diamond.begin_block(d_merge);
        diamond.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X0),
                imm12: 2,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        diamond.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        diamond.end_block(d_merge);

        diamond.begin_block(d_cold);
        diamond.push_inst(MOp::MovImm { dst: MReg::p(PReg::X0), imm: 7, wide: true }, MInstFlags::NONE);
        diamond.push_inst(MOp::B { target: d_merge }, MInstFlags::TERMINATOR);
        diamond.end_block(d_cold);

        let d_enc = mencode(&diamond, alloc).expect("mencode diamond merge must succeed");
        assert_eq!(d_enc.block_word_off.as_slice(), &[0, 1, 2, 4]);
        assert_eq!(d_enc.words.len(), 6);
        assert_eq!(d_enc.words[0], enc::sf(true, enc::cbz(X0, 4)));
        assert_eq!(d_enc.words[1], enc::movz(X0, 1, 0));
        assert_eq!(d_enc.words[2], enc::sf(true, enc::add_imm(X0, X0, 2, false)));
        assert_eq!(d_enc.words[3], enc::ret());
        assert_eq!(d_enc.words[4], enc::movz(X0, 7, 0));
        assert_eq!(d_enc.words[5], enc::b(-3));
        assert!(
            !d_enc.words.iter().any(|&w| w == enc::nop()),
            "non-loop diamond merge on fallthrough must emit ZERO alignment NOPs"
        );

        // Case 3 (`block_align16 = false` + backward branch after an unconditional terminator):
        // preceded by unconditional `B` (even across an intervening empty block) -> aligned
        // cleanly to a 16-byte boundary with zero executed `NOP`s!
        let mut back_uncond = new_post_ra(Os::MacOs);
        let bu_entry = back_uncond.new_block(false);
        let bu_empty = back_uncond.new_block(false);
        let bu_merge = back_uncond.new_block(false);
        let bu_cold = back_uncond.new_block(false);

        back_uncond.begin_block(bu_entry);
        back_uncond.push_inst(
            MOp::Cbz { rt: MReg::p(PReg::X0), target: bu_cold, wide: true },
            MInstFlags::TERMINATOR,
        );
        back_uncond.push_inst(MOp::B { target: bu_merge }, MInstFlags::TERMINATOR);
        back_uncond.end_block(bu_entry);

        back_uncond.begin_block(bu_empty);
        back_uncond.end_block(bu_empty);

        back_uncond.begin_block(bu_merge);
        back_uncond.push_inst(
            MOp::AddImm {
                dst: MReg::p(PReg::X0),
                rn: MReg::p(PReg::X0),
                imm12: 2,
                shift12: false,
                wide: true,
                sets_flags: false,
            },
            MInstFlags::NONE,
        );
        back_uncond.push_inst(MOp::Ret, MInstFlags::TERMINATOR);
        back_uncond.end_block(bu_merge);

        back_uncond.begin_block(bu_cold);
        back_uncond.push_inst(MOp::MovImm { dst: MReg::p(PReg::X0), imm: 7, wide: true }, MInstFlags::NONE);
        back_uncond.push_inst(MOp::B { target: bu_merge }, MInstFlags::TERMINATOR);
        back_uncond.end_block(bu_cold);

        let bu_enc = mencode(&back_uncond, alloc).expect("mencode uncond backedge target must succeed");
        assert_eq!(bu_enc.block_word_off.as_slice(), &[0, 2, 4, 6]);
        assert_eq!(bu_enc.words.len(), 8);
        assert_eq!(bu_enc.words[0], enc::sf(true, enc::cbz(X0, 6)));
        assert_eq!(bu_enc.words[1], enc::b(3));
        assert_eq!(bu_enc.words[2], enc::nop());
        assert_eq!(bu_enc.words[3], enc::nop());
        assert_eq!(bu_enc.words[4], enc::sf(true, enc::add_imm(X0, X0, 2, false)));
        assert_eq!(bu_enc.words[5], enc::ret());
        assert_eq!(bu_enc.words[6], enc::movz(X0, 7, 0));
        assert_eq!(bu_enc.words[7], enc::b(-3));
    }

    #[test]
    fn mblock_and_minst_helpers_have_expected_identities() {
        assert_eq!(MBlock(3).0, 3);
        assert_eq!(MInst(7).0, 7);
        assert_eq!(MCond::from_enc(MCond::Hs.to_enc()), MCond::Hs);
        assert_eq!(MCond::Lt.invert(), MCond::Ge);
    }
}
