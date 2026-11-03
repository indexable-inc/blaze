//! The x86 instruction table.
//!
//! Operand patterns are written in **Intel order** (destination first). The
//! AT&T front end reverses its operands before matching, so there is only one
//! table.
//!
//! The tables themselves live in submodules, one per instruction-set family,
//! each contributing its entries through an `install` function. Splitting them
//! keeps any one file readable: the SIMD families alone outnumber the base
//! integer instruction set several times over.

pub(crate) mod apx;
pub mod avx;
pub(crate) mod avx10;
pub mod avx512;
pub(crate) mod avx512x;
pub mod base;
pub(crate) mod bmi;
pub(crate) mod cmpalias;
pub(crate) mod fma;
pub(crate) mod fp16;
pub(crate) mod lenalias;
pub mod mmx;
pub mod sse;
pub(crate) mod sys;
pub(crate) mod vexext;
pub mod x87;
pub(crate) mod xop;

use super::reg::{Reg, RegClass};
use std::collections::HashMap;
use std::sync::OnceLock;

/// A non-general-purpose register file an operand slot can name.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Vk {
    /// `mm0`-`mm7`.
    Mm,
    Xmm,
    Ymm,
    Zmm,
    /// The AVX-512 opmask registers `k0`-`k7`, as an ordinary operand rather
    /// than as a `{k1}` writemask decorator.
    K,
    /// The AMX tile registers `tmm0`-`tmm7`.
    Tmm,
}

impl Vk {
    /// Register width in bytes, which is also the natural width of a memory
    /// operand in the same slot.
    pub fn width(self) -> u8 {
        match self {
            Vk::Mm | Vk::K => 8,
            // A tile has no fixed size, and neither has its memory operand.
            Vk::Tmm => 0,
            Vk::Xmm => 16,
            Vk::Ymm => 32,
            Vk::Zmm => 64,
        }
    }

    pub fn class(self) -> RegClass {
        match self {
            Vk::Mm => RegClass::Mmx,
            Vk::Xmm => RegClass::Xmm,
            Vk::Ymm => RegClass::Ymm,
            Vk::Zmm => RegClass::Zmm,
            Vk::K => RegClass::Mask,
            Vk::Tmm => RegClass::Tmm,
        }
    }

    pub fn accepts(self, r: Reg) -> bool {
        r.class == self.class()
    }
}

/// What an operand slot accepts. Widths are in bytes.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Op {
    /// Register or memory of the given width.
    Rm(u8),
    /// Register only.
    R(u8),
    /// Memory only; width 0 means "any, size irrelevant" (as for `lea`).
    M(u8),
    /// Immediate encoded in this many bytes.
    Imm(u8),
    /// One immediate byte, sign-extended to the operation width.
    Imm8s,
    /// Branch displacement of this many bytes.
    Rel(u8),
    /// A specific register, by name.
    Fixed(&'static str),
    /// The literal constant 1, as in `shl $1, %eax`.
    One,
    /// The literal constant 3, which makes `int $3` the one-byte `int3`.
    Three,
    /// Register or memory operand used indirectly (`jmp *%rax`).
    IndirectRm(u8),
    /// A vector or mask register, encoded in ModRM.reg.
    V(Vk),
    /// A vector or mask register, or memory, encoded in ModRM.rm. The second
    /// field is the memory operand's width in bytes; `0` means the register
    /// width, which is what all the full-width forms want.
    Vm(Vk, u8),
    /// The non-destructive source VEX and EVEX carry in `vvvv`.
    Nds(Vk),
    /// A general register of this width in `vvvv`, as BMI's `andn` and
    /// `shlx` take one.
    NdsR(u8),
    /// A register named by the top four bits of a trailing immediate byte, as
    /// `vblendvps` does with its selector. Beside an `Imm(1)`, as in XOP's
    /// `vpermil2ps`, the two share the byte: the immediate is its low nibble.
    Is4(Vk),
    /// A gather/scatter memory operand, whose SIB index is a vector register
    /// of this class rather than a GPR.
    Vsib(Vk),
    /// A segment register, encoded in ModRM.reg.
    Seg,
    /// A control register, encoded in ModRM.reg.
    Cr,
    /// A debug register, encoded in ModRM.reg.
    Dr,
    /// An x87 stack register `st(i)`, added to the last opcode byte.
    St,
    /// An absolute address with no base or index, carried as an
    /// address-sized field in place of ModRM (`mov 0x1000, %eax` as `A1`).
    /// The width is that of the data moved.
    Moffs(u8),
    /// A direct far pointer: an offset of the operand size, then a 16-bit
    /// segment selector.
    Far,
    /// The memory operand of a far indirect branch (`ljmp *(%eax)`), in AT&T
    /// syntax with or without `*`, and unsized or `fword` in Intel syntax.
    FarM,
    /// The same, but only when written `fword ptr`, which is how Intel syntax
    /// tells `jmp fword ptr [eax]` from a near `jmp [eax]`.
    Fword,
    /// A far pointer with a 16-bit offset, which GNU as's Intel syntax reads
    /// `jmp dword ptr [bx]` as outside 32-bit mode: four bytes are a near
    /// 32-bit target only where that is the operand size.
    FarDword,
    /// The port register of `in` and `out`: `%dx`, which AT&T also writes as
    /// `(%dx)`.
    Dx,
    /// The source or destination a string instruction was written with, as in
    /// `movsb (%esi), %es:(%edi)`, of this width. Neither is encoded: they only
    /// name the address size, and a segment for the source.
    StrSrc(u8),
    StrDst(u8),
}

impl Op {
    pub fn width(self) -> u8 {
        match self {
            Op::Rm(w) | Op::R(w) | Op::M(w) | Op::Imm(w) | Op::IndirectRm(w) | Op::NdsR(w) => w,
            Op::Imm8s => 1,
            Op::Rel(w) => w,
            Op::One | Op::Three => 0,
            Op::Fixed(_) => 0,
            Op::V(k) | Op::Nds(k) | Op::Is4(k) | Op::Vsib(k) => k.width(),
            Op::Vm(k, 0) => k.width(),
            Op::Vm(_, w) => w,
            Op::Moffs(w) | Op::StrSrc(w) | Op::StrDst(w) => w,
            Op::Seg
            | Op::Cr
            | Op::Dr
            | Op::St
            | Op::Far
            | Op::FarM
            | Op::Fword
            | Op::FarDword
            | Op::Dx => 0,
        }
    }
}

/// How ModRM is formed.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ModRm {
    /// No ModRM byte.
    None,
    /// `/r`: the reg field holds a register operand.
    Reg,
    /// `/digit`: the reg field is a fixed opcode extension.
    Ext(u8),
}

pub const PLUSREG: u32 = 1 << 0;
/// Operand size defaults to 64 bits in long mode (push, pop, jmp, call, ret).
pub const DEF64: u32 = 1 << 1;
/// Only encodable in 64-bit mode.
pub const ONLY64: u32 = 1 << 2;
/// Not encodable in 64-bit mode.
pub const NO64: u32 = 1 << 3;
/// The immediate is an absolute 64-bit value (`movabs`).
pub const IMM64: u32 = 1 << 4;
/// Not usable when every register operand is the accumulator. `xchg` needs
/// this: `xchg eax, eax` must not encode as `90`, which is `nop` and does not
/// clear the upper half of `rax`.
pub const NOTACC: u32 = 1 << 5;
/// A 64-bit form that needs no REX.W, because the opcode already fixes the
/// width: a control or debug register is as wide as the mode on its own, the
/// 64-bit `0f 01` system rows have no other size, and `lar` and `lsl` produce
/// at most 32 bits whatever register they are given.
pub const NO_REX_W: u32 = 1 << 6;
/// The instruction accepts embedded rounding control (`{rn-sae}` and its
/// siblings). Only the 512-bit and scalar forms of the operations whose result
/// depends on the rounding mode do: `vaddps` yes, `vmaxps` no.
pub const EVEX_ER: u32 = 1 << 7;
/// This EVEX form takes no writemask, so `{k1}` on it is an error. Most EVEX
/// instructions are maskable; the handful that are not (the non-temporal
/// stores, `vcomiss`, `vmovd`) set this.
pub const NOMASK: u32 = 1 << 8;
/// The instruction accepts `{sae}` alone: it can raise floating-point
/// exceptions but has no rounding to control (`vmaxps`, `vcmpps`, `vcomiss`).
pub const EVEX_SAE: u32 = 1 << 9;
/// The writemask is mandatory. Gathers and scatters use it as the per-element
/// "still to do" set, so there is no unmasked form to fall back to.
pub const NEEDS_MASK: u32 = 1 << 10;
/// The operand size is only for matching an AT&T suffix or an Intel size
/// keyword; no `66` prefix follows from it. Moves to and from segment
/// registers are always 16 bits wide, whatever the mode, and so is `arpl`.
pub const NO66: u32 = 1 << 11;
/// The instruction starts with the `9B` (`fwait`) prefix, which goes before
/// any other: `fstcw` is `fwait` then `fnstcw`.
pub const WAIT: u32 = 1 << 12;
/// The instruction's implicit address size is 16 or 32 bits, as the counter
/// register of `jcxz` and `jecxz` is, so another mode needs a `67` prefix.
pub const ADDR16: u32 = 1 << 13;
pub const ADDR32: u32 = 1 << 14;
/// The form exists only in AT&T syntax, or only in Intel syntax. The x87
/// `fsub`/`fsubr` family needs these: GNU as's AT&T syntax swaps the two
/// register forms with `st(i)` as destination, a historical mistake that
/// every AT&T assembler has had to keep, while its Intel syntax encodes them
/// as the manual does.
pub const ATT_ONLY: u32 = 1 << 15;
pub const INTEL_ONLY: u32 = 1 << 16;
/// A general register operand goes in ModRM.rm even beside a vector
/// register in ModRM.reg, as in `movd %xmm0, %rax`, whose r/m operand is
/// register-only in that form.
pub const R_IN_RM: u32 = 1 << 17;
/// The destination register must differ from both sources. AVX-512FP16's
/// complex multiplications read their operands in pairs of elements and
/// would overwrite one half before reading the other; GNU as refuses the
/// overlap, where llvm-mc assembles it, and rsasm follows GNU as.
pub(crate) const DISTINCT_DEST: u32 = 1 << 18;
/// The memory operand is always written with a SIB byte, and so cannot be
/// RIP-relative: AMX's tile loads and stores take their stride from the
/// index register, and have no encoding without one.
pub(crate) const SIBMEM: u32 = 1 << 19;
/// The form has no REX2 encoding, so an `r16`-`r31` operand cannot reach it:
/// GNU as's `noegpr`. The map an opcode lives in decides most of these —
/// REX2 covers the one-byte and `0F` maps and nothing else — so the flag is
/// only for the one family that is in reach and still excluded, the extended
/// state saves, which address memory with `rbx` as well as with the register
/// they are given.
pub(crate) const NO_REX2: u32 = 1 << 21;
/// The row writes a new destination register, which the extended EVEX prefix
/// carries in `vvvv` with the `ND` bit set. `setzu`/`imulzu` set the same bit
/// to ask for the zeroed upper half, and `push2`/`pop2` because `vvvv` is a
/// register they write.
pub(crate) const APX_ND: u32 = 1 << 22;
/// `{nf}` may be written on this row, setting `EVEX.NF` so the operation
/// leaves the flags alone. Only the rows Intel promoted with a no-flags form
/// have it: `add` yes, `adc` and `rcl` no.
pub(crate) const APX_NF: u32 = 1 << 23;
/// The row *is* a no-flags encoding, with `EVEX.NF` set whether or not `{nf}`
/// was written. `cfcmov`'s store and three-operand forms are told from its
/// load form by that bit alone.
pub(crate) const APX_NF_ON: u32 = 1 << 24;
/// The row is encodable only with the REX2 prefix, which is what tells it
/// from the instruction it shares an opcode with: `pushp` is `push` under
/// REX2.W, and `jmpabs` is the long-gone `mov` from a 64-bit absolute address.
pub(crate) const APX_REX2: u32 = 1 << 25;
/// `rsp` is not one of the registers this row names. `push2` and `pop2` move
/// the stack pointer themselves, so it cannot be one of the pair.
pub(crate) const NO_RSP: u32 = 1 << 26;
/// The two registers in `vvvv` and r/m must differ: `pop2` writes both, and
/// one pop would otherwise be lost. `push2` only reads them, and GNU as
/// accepts `push2 %rax, %rax`.
pub(crate) const DISTINCT_PAIR: u32 = 1 << 27;
/// A 64-bit form GNU as writes without REX.W although the register asked for
/// it. `xchg rax, rax` is the one case: GNU as spells it `90`, the canonical
/// `nop`, and NASM writes the `48 90` the operands name. Both do nothing, so
/// each dialect keeps the spelling its own reference produces.
pub(crate) const NO_REX_W_GAS: u32 = 1 << 20;

/// Which prefix family carries the instruction.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum Enc {
    /// Opcode bytes as written, with a legacy mandatory prefix if any.
    #[default]
    Legacy,
    /// Two- or three-byte VEX, chosen by whichever fits.
    Vex,
    /// Four-byte EVEX.
    Evex,
    /// Four-byte EVEX in APX's extended layout: the same `62` prefix with the
    /// general-register fields in place of the vector ones. See the note on
    /// [`crate::arch::x86::insn::apx`].
    Apx,
}

/// EVEX *tuple type*: how a compressed 8-bit displacement is scaled.
///
/// EVEX replaces the plain `disp8` with `disp8 * N`, so a byte can still reach
/// the whole of a 512-bit stride. `N` is not the operand size: it is the size
/// of the *memory access the instruction actually makes*, which depends on the
/// vector length, on `EVEX.W`, and on whether a `{1toN}` broadcast turned a
/// full-width load into a scalar one. The tuple type is the manual's name for
/// the rule that decides it, and each opcode is documented with exactly one.
///
/// Getting `N` wrong yields an instruction that assembles cleanly and touches
/// the wrong address, which is why this is spelled out rather than guessed.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum Tuple {
    /// Not an EVEX form, or an EVEX form with no memory operand.
    #[default]
    None,
    /// Full Vector: the whole register, or one element under broadcast.
    Fv,
    /// Half Vector: half the register, or one 32-bit element broadcast.
    Hv,
    /// Full Vector Mem: the whole register, never broadcast.
    Fvm,
    /// Half Mem, Quarter Mem, Eighth Mem: the `vpmov*` conversions, which
    /// read or write a fraction of the register.
    Hvm,
    Qvm,
    Ovm,
    /// Tuple1 Scalar: one element, sized by `EVEX.W` (4 or 8 bytes).
    T1s,
    /// Tuple1 Scalar with a fixed byte, word or dword element: `vpbroadcastb`
    /// and `w`, and the single-precision scalars whose `EVEX.W` sizes a
    /// general register instead (`vcvtss2usi %xmm0, %rax`).
    T1s8,
    T1s16,
    T1s32,
    /// And a fixed quadword, for the double-precision scalar converted to a
    /// 32-bit register (`vcvtsd2usi %xmm0, %eax`), whose `W` is 0.
    T1s64,
    /// Tuple4: four elements, sized by `EVEX.W` — the `32x4`/`64x4` inserts,
    /// extracts and broadcasts.
    T4,
    /// `vmovddup`: 8 bytes at 128 bits, the whole register above that.
    Dup,
    /// Mem128: always sixteen bytes, whatever the vector length. The shifts
    /// that take their count from an `xmm` register read one of these.
    M128,
    /// Tuple2 and Tuple8: two or eight elements, sized by `EVEX.W` — the
    /// `32x2`/`64x2` and `32x8` sub-vector broadcasts, inserts and extracts.
    T2,
    T8,
    /// Full, Half and Quarter Vector with word elements: AVX-512FP16's packed
    /// forms, whose broadcast repeats a two-byte half-precision float, and
    /// its conversions that widen half or a quarter of the register.
    Fvw,
    Hvw,
    Qvw,
}

impl Tuple {
    /// The disp8 scale factor for this tuple at `vbytes` vector bytes.
    ///
    /// `w` is `EVEX.W` and `broadcast` says whether a `{1toN}` decorator turned
    /// the memory operand into a single element.
    pub fn scale(self, vbytes: u32, w: bool, broadcast: bool) -> Option<u32> {
        let elem = if w { 8 } else { 4 };
        Some(match self {
            Tuple::None => return None,
            Tuple::Fv => {
                if broadcast {
                    elem
                } else {
                    vbytes
                }
            }
            Tuple::Hv => {
                if broadcast {
                    4
                } else {
                    vbytes / 2
                }
            }
            Tuple::Fvw | Tuple::Hvw | Tuple::Qvw if broadcast => 2,
            Tuple::Fvw => vbytes,
            Tuple::Hvw => vbytes / 2,
            Tuple::Qvw => vbytes / 4,
            Tuple::Fvm => vbytes,
            Tuple::Hvm => vbytes / 2,
            Tuple::Qvm => vbytes / 4,
            Tuple::Ovm => vbytes / 8,
            Tuple::T1s => elem,
            Tuple::T1s8 => 1,
            Tuple::T1s16 => 2,
            Tuple::T1s32 => 4,
            Tuple::T1s64 => 8,
            Tuple::T2 => elem * 2,
            Tuple::T4 => elem * 4,
            Tuple::T8 => elem * 8,
            Tuple::M128 => 16,
            Tuple::Dup => {
                if vbytes == 16 {
                    8
                } else {
                    vbytes
                }
            }
        })
    }

    /// True if this tuple's memory operand may carry a `{1toN}` broadcast.
    pub fn broadcastable(self) -> bool {
        matches!(
            self,
            Tuple::Fv | Tuple::Hv | Tuple::Fvw | Tuple::Hvw | Tuple::Qvw
        )
    }
}

#[derive(Clone, Debug)]
pub struct Def {
    pub ops: Vec<Op>,
    /// Mandatory prefix emitted before REX: 0x66, 0xF2 or 0xF3. For VEX and
    /// EVEX the same value picks the `pp` field instead of being emitted.
    pub pfx: u8,
    /// Legacy: every opcode byte including the `0F` escapes. VEX and EVEX:
    /// just the final opcode byte, since the escape lives in `map`.
    pub opcode: Vec<u8>,
    pub modrm: ModRm,
    /// Operation width in bits: 0 (irrelevant), 8, 16, 32 or 64. Drives the
    /// 0x66 prefix and REX.W (or VEX.W / EVEX.W).
    pub opsize: u8,
    pub flags: u32,
    pub enc: Enc,
    /// VEX/EVEX opcode map: 1 = `0F`, 2 = `0F 38`, 3 = `0F 3A`, and the
    /// EVEX-only 5 and 6. Maps 8 to 10 are AMD's XOP space: a VEX row there
    /// is written with XOP's `8F` escape in place of VEX's `C4`.
    pub map: u8,
    /// Vector length in bits: 128, 256 or 512, for the VEX/EVEX `L` bits.
    pub vlen: u16,
    pub tuple: Tuple,
    /// `ccmp`/`ctest`'s source condition code, which the extended EVEX prefix
    /// carries where AVX-512 keeps the writemask.
    pub scc: Option<u8>,
    /// A byte the source never writes, emitted *after* the ModRM, any
    /// displacement and any immediate. 3DNow! selects the operation with one,
    /// and the named compare predicates (`cmpeqps`) fold their immediate into
    /// the mnemonic the same way.
    pub suffix: Option<u8>,
}

impl Def {
    pub fn new(ops: Vec<Op>, opcode: Vec<u8>, modrm: ModRm, opsize: u8) -> Def {
        Def {
            ops,
            pfx: 0,
            opcode,
            modrm,
            opsize,
            flags: 0,
            enc: Enc::Legacy,
            map: 0,
            vlen: 0,
            tuple: Tuple::None,
            scc: None,
            suffix: None,
        }
    }

    pub fn flags(mut self, f: u32) -> Def {
        self.flags |= f;
        self
    }

    /// The mandatory `66`/`F2`/`F3` prefix, or the VEX/EVEX `pp` it implies.
    pub fn pfx(mut self, p: u8) -> Def {
        self.pfx = p;
        self
    }

    pub fn map(mut self, m: u8) -> Def {
        self.map = m;
        self
    }

    pub fn vex(mut self, vlen: u16) -> Def {
        self.enc = Enc::Vex;
        self.vlen = vlen;
        self
    }

    pub fn evex(mut self, vlen: u16, tuple: Tuple) -> Def {
        self.enc = Enc::Evex;
        self.vlen = vlen;
        self.tuple = tuple;
        self
    }

    /// True if the row has a REX2 encoding, which is what lets an `r16`-`r31`
    /// operand ride on a legacy prefix rather than on the extended EVEX one.
    /// REX2 reaches the one-byte and `0F` opcode maps and no further, and two
    /// families are excluded even there; see [`NO_REX2`].
    pub(crate) fn rex2_ok(&self) -> bool {
        self.enc == Enc::Legacy
            && self.flags & NO_REX2 == 0
            && matches!(self.opcode.as_slice(), [_] | [0x0f, _])
            // A relative branch has no register field for the extra bits to
            // fill, and GNU as refuses `{rex2}` on one rather than write a
            // prefix that says nothing.
            && !self.ops.iter().any(|o| matches!(o, Op::Rel(_)))
    }

    /// An APX row: the extended EVEX prefix, in `map`.
    pub(crate) fn apx(mut self, map: u8) -> Def {
        self.enc = Enc::Apx;
        self.map = map;
        self
    }

    /// True if an `r16`-`r31` operand is encodable in this row: either the
    /// prefix has APX's fourth register-number bit, or REX2 can carry it.
    pub(crate) fn egpr_ok(&self) -> bool {
        matches!(self.enc, Enc::Apx | Enc::Evex) || self.rex2_ok()
    }

    /// `ccmp`/`ctest`'s source condition code.
    pub(crate) fn scc(mut self, n: u8) -> Def {
        self.scc = Some(n);
        self
    }

    pub fn suffix(mut self, s: u8) -> Def {
        self.suffix = Some(s);
        self
    }

    /// True when `VEX.W` / `EVEX.W` is set, which the tables express as a
    /// 64-bit operand size. `push2` is the exception: its operands are
    /// quadwords and the bit means the `push2p` hint instead, so the rows that
    /// fix `W` at zero say so with [`NO_REX_W`].
    pub fn vex_w(&self) -> bool {
        self.opsize == 64 && self.flags & NO_REX_W == 0
    }

    /// The `N` of the `{1toN}` this row's memory operand broadcasts with, or
    /// `None` if it cannot broadcast: the element count of the memory the
    /// full-width form would read.
    pub(crate) fn broadcast_count(&self) -> Option<u32> {
        if !self.tuple.broadcastable() {
            return None;
        }
        let vbytes = self.vlen as u32 / 8;
        Some(match self.tuple {
            // A half-vector source is half the register, in dword elements.
            Tuple::Hv => vbytes / 2 / 4,
            // Half-precision elements are words.
            Tuple::Fvw => vbytes / 2,
            Tuple::Hvw => vbytes / 2 / 2,
            Tuple::Qvw => vbytes / 4 / 2,
            _ => vbytes / if self.vex_w() { 8 } else { 4 },
        })
    }
}

pub fn d(ops: Vec<Op>, opcode: &[u8], modrm: ModRm, opsize: u8) -> Def {
    Def::new(ops, opcode.to_vec(), modrm, opsize)
}

/// The 16 condition codes, in `tttn` order, with every accepted spelling.
#[rustfmt::skip]
pub const CONDITIONS: &[(&str, u8)] = &[
    ("o", 0x0),
    ("no", 0x1),
    ("b", 0x2), ("c", 0x2), ("nae", 0x2),
    ("ae", 0x3), ("nb", 0x3), ("nc", 0x3),
    ("e", 0x4), ("z", 0x4),
    ("ne", 0x5), ("nz", 0x5),
    ("be", 0x6), ("na", 0x6),
    ("a", 0x7), ("nbe", 0x7),
    ("s", 0x8),
    ("ns", 0x9),
    ("p", 0xa), ("pe", 0xa),
    ("np", 0xb), ("po", 0xb),
    ("l", 0xc), ("nge", 0xc),
    ("ge", 0xd), ("nl", 0xd),
    ("le", 0xe), ("ng", 0xe),
    ("g", 0xf), ("nle", 0xf),
];

/// Widths that the generic "r/m, r" style patterns are generated for.
pub const WIDTHS: [u8; 3] = [2, 4, 8];

pub fn opsize_bits(w: u8) -> u8 {
    w * 8
}

/// The table under construction, so a family module can append to a mnemonic
/// another family already defined without silently replacing it.
pub type Tbl = HashMap<&'static str, Vec<Def>>;

/// Appends `defs` to `mnem`, creating the entry if needed.
///
/// Order within an entry is preference order, and `add` keeps it: a family
/// installed later offers its forms only after the earlier ones have been
/// tried. `movq` relies on that — the GPR forms must win over the MMX ones.
pub fn add(t: &mut Tbl, mnem: &'static str, defs: Vec<Def>) {
    t.entry(mnem).or_default().extend(defs);
}

fn build() -> Tbl {
    let mut t: Tbl = HashMap::new();
    base::install(&mut t);
    x87::install(&mut t);
    mmx::install(&mut t);
    sse::install(&mut t);
    avx::install(&mut t);
    vexext::install(&mut t);
    bmi::install(&mut t);
    sys::install(&mut t);
    xop::install(&mut t);
    avx512::install(&mut t);
    avx512x::install(&mut t);
    vexext::install_late(&mut t);
    fma::install(&mut t);
    fp16::install(&mut t);
    avx10::install(&mut t);
    cmpalias::install(&mut t);
    lenalias::install(&mut t);
    // Last, because it reads the rows it promotes out of the finished table,
    // and because a promoted row must be offered only after the legacy one.
    apx::install(&mut t);
    t
}

pub fn table() -> &'static HashMap<&'static str, Vec<Def>> {
    static TABLE: OnceLock<HashMap<&'static str, Vec<Def>>> = OnceLock::new();
    TABLE.get_or_init(build)
}

pub fn lookup(mnemonic: &str) -> Option<&'static [Def]> {
    table().get(mnemonic).map(|v| v.as_slice())
}

/// True if `name` names an instruction, ignoring AT&T size suffixes.
pub fn is_mnemonic(name: &str) -> bool {
    table().contains_key(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_covers_the_expected_groups() {
        for m in [
            "add", "sub", "mov", "lea", "jmp", "je", "setne", "cmovg", "imul", "shl", "ret",
        ] {
            assert!(is_mnemonic(m), "missing `{m}`");
        }
        assert!(!is_mnemonic("nosuchinsn"));
    }

    #[test]
    fn jmp_offers_a_short_and_a_near_form() {
        let defs = lookup("jmp").unwrap();
        assert!(defs.iter().any(|x| x.ops == [Op::Rel(1)]));
        assert!(defs.iter().any(|x| x.ops == [Op::Rel(4)]));
    }

    #[test]
    fn alu_prefers_sign_extended_imm8() {
        let defs = lookup("add").unwrap();
        let i8_pos = defs
            .iter()
            .position(|x| x.ops == [Op::Rm(4), Op::Imm8s])
            .unwrap();
        let i32_pos = defs
            .iter()
            .position(|x| x.ops == [Op::Rm(4), Op::Imm(4)])
            .unwrap();
        assert!(i8_pos < i32_pos, "imm8 form must be matched first");
    }

    /// Every row across every family, with its mnemonic.
    fn all_rows() -> impl Iterator<Item = (&'static str, &'static Def)> {
        table()
            .iter()
            .flat_map(|(m, defs)| defs.iter().map(move |d| (*m, d)))
    }

    #[test]
    fn simd_families_are_installed() {
        for m in [
            "paddb",
            "pfadd",
            "femms",
            "addps",
            "pshufb",
            "pcmpistri",
            "aesenc",
            "crc32",
            "vaddps",
            "vgatherdps",
            "vfmadd231sd",
            "vmovdqa32",
            "vpternlogd",
            "kmovw",
        ] {
            assert!(is_mnemonic(m), "missing `{m}`");
        }
    }

    #[test]
    fn prefixed_rows_are_well_formed() {
        for (m, d) in all_rows() {
            if d.enc == Enc::Legacy {
                assert!(
                    d.vlen == 0 && d.tuple == Tuple::None,
                    "`{m}`: legacy row {d:?}"
                );
                continue;
            }
            // APX's extended EVEX has neither a vector length nor a
            // compressed displacement, and its maps are the promoted VEX ones
            // and map 4.
            if d.enc == Enc::Apx {
                assert!(matches!(d.map, 1..=4), "`{m}`: bad map in {d:?}");
                assert_eq!(d.opcode.len(), 1, "`{m}`: APX opcode is one byte");
                assert!(
                    d.vlen == 0 && d.tuple == Tuple::None && d.suffix.is_none(),
                    "`{m}`: {d:?}"
                );
                continue;
            }
            // Maps 5 and 6 are EVEX-only, and hold AVX-512FP16; 8 to 10 are
            // XOP's, which only VEX-style rows use.
            let map_ok = match d.enc {
                Enc::Evex => matches!(d.map, 1..=3 | 5 | 6),
                _ => matches!(d.map, 1..=3 | 8..=10),
            };
            assert!(map_ok, "`{m}`: bad map in {d:?}");
            assert_eq!(d.opcode.len(), 1, "`{m}`: VEX/EVEX opcode is one byte");
            assert!(matches!(d.vlen, 128 | 256 | 512), "`{m}`: bad length");
            // A VEX or EVEX suffix byte is an immediate folded into the name
            // (`vcmpeqps`, `vpcmpltud`, `vpcomgeb`, `vpclmullqhqdq`), or
            // `tilerelease`'s fixed ModRM; 3DNow! is the only legacy family
            // with one.
            assert!(
                d.suffix.is_none()
                    || ["vcmp", "vpcmp", "vpcom", "vpclmul", "tilerelease"]
                        .iter()
                        .any(|p| m.starts_with(p)),
                "`{m}`: unexpected suffix byte"
            );
            if d.enc == Enc::Vex {
                assert!(d.vlen != 512 && d.tuple == Tuple::None, "`{m}`: {d:?}");
            }
        }
    }

    #[test]
    fn every_evex_memory_form_has_a_tuple_type() {
        // Without one the displacement cannot be compressed correctly, so the
        // encoder would refuse the instruction at the first memory operand.
        for (m, d) in all_rows() {
            let takes_mem = d
                .ops
                .iter()
                .any(|o| matches!(o, Op::Vm(..) | Op::M(_) | Op::Rm(_) | Op::Vsib(_)));
            if d.enc == Enc::Evex && takes_mem {
                assert_ne!(d.tuple, Tuple::None, "`{m}`: {d:?}");
            }
        }
    }

    /// The fields only the extended EVEX prefix has room for, and the rows
    /// that have to be reachable at all.
    #[test]
    fn apx_fields_are_only_on_apx_rows() {
        for (m, d) in all_rows() {
            let apx = APX_ND | APX_NF | APX_NF_ON;
            if d.flags & apx != 0 || d.scc.is_some() {
                assert_eq!(d.enc, Enc::Apx, "`{m}`: {d:?}");
            }
            // `{nf}` and the no-flags encodings are different things: the
            // first chooses a row, the second is what the row already is.
            assert!(
                d.flags & (APX_NF | APX_NF_ON) != APX_NF | APX_NF_ON,
                "`{m}`: both `{{nf}}` forms"
            );
            // A row that only REX2 can carry has to be in REX2's reach.
            if d.flags & APX_REX2 != 0 {
                assert!(d.rex2_ok(), "`{m}`: {d:?}");
            }
        }
    }

    #[test]
    fn apx_promotes_the_families_it_claims() {
        for m in [
            "ccmpe", "ctestz", "cfcmovne", "setzub", "imulzu", "push2", "pop2", "push2p", "pop2p",
            "pushp", "popp", "jmpabs", "clr",
        ] {
            assert!(is_mnemonic(m), "missing `{m}`");
        }
        // `ccmp` has no parity condition: the flags it may assume are only
        // the four `{dfv=...}` names.
        assert!(!is_mnemonic("ccmpp"));
        assert!(!is_mnemonic("ctestnp"));
        assert!(is_mnemonic("cfcmovp"));
        // One promoted row of each kind: a legacy opcode moved into map 4, a
        // VEX row repeated, and a new destination register.
        let has = |m: &str, f: &dyn Fn(&Def) -> bool| lookup(m).unwrap().iter().any(f);
        assert!(has("movbe", &|d| d.enc == Enc::Apx && d.opcode == [0x60]));
        assert!(has("andn", &|d| d.enc == Enc::Apx && d.map == 2));
        assert!(has("cmpexadd", &|d| d.enc == Enc::Apx && d.map == 2));
        assert!(has("add", &|d| d.flags & APX_ND != 0 && d.ops.len() == 3));
        assert!(has("shld", &|d| d.flags & APX_ND != 0 && d.ops.len() == 4));
    }

    #[test]
    fn rounding_is_only_offered_where_the_prefix_has_room_for_it() {
        for (m, d) in all_rows() {
            if d.flags & (EVEX_ER | EVEX_SAE) != 0 {
                assert_eq!(d.enc, Enc::Evex, "`{m}`: {d:?}");
                // Packed forms need the full 512 bits; scalars ignore L'L.
                let scalar = matches!(
                    d.tuple,
                    Tuple::T1s | Tuple::T1s16 | Tuple::T1s32 | Tuple::T1s64 | Tuple::None
                );
                assert!(d.vlen == 512 || scalar, "`{m}`: {d:?}");
            }
        }
    }
}
