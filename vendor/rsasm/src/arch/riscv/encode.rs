//! Instruction words, immediate fields and the fixups that fill them in.
//!
//! Almost no RISC-V immediate is contiguous: the assembler builds the word
//! with the immediate zeroed and hands the placement to a [`FieldEncoding::
//! Scatter`](crate::section::FieldEncoding::Scatter) function, which is the
//! same code path whether the value is known now or comes back from the
//! linker.

use super::reloc;
use crate::expr::ExprRef;
use crate::reloc::RelocClass;
use crate::section::{Fixup, FixupKind, LinkValue, RelocSymbol, Variant};
use crate::source::Span;

// ---- field placement ------------------------------------------------------

pub fn rd(w: u32, r: u32) -> u32 {
    (w & !(31 << 7)) | ((r & 31) << 7)
}

pub fn rs1(w: u32, r: u32) -> u32 {
    (w & !(31 << 15)) | ((r & 31) << 15)
}

pub fn rs2(w: u32, r: u32) -> u32 {
    (w & !(31 << 20)) | ((r & 31) << 20)
}

pub fn rs3(w: u32, r: u32) -> u32 {
    (w & !(31 << 27)) | ((r & 31) << 27)
}

pub fn funct3(w: u32, f: u32) -> u32 {
    (w & !(7 << 12)) | ((f & 7) << 12)
}

// ---- scatter functions ----------------------------------------------------
//
// Each takes the word already emitted (read back in the target's byte order)
// and the resolved value, and returns the patched word.

/// I-type: `imm[11:0]` in bits 31:20.
pub fn i_imm(word: u64, v: i64) -> u64 {
    (word & 0x000f_ffff) | ((v as u64 & 0xfff) << 20)
}

/// S-type: `imm[11:5]` in bits 31:25 and `imm[4:0]` in bits 11:7.
pub fn s_imm(word: u64, v: i64) -> u64 {
    let v = v as u64;
    (word & 0x01ff_f07f) | ((v >> 5) & 0x7f) << 25 | (v & 0x1f) << 7
}

/// B-type: `imm[12|10:5]` in bits 31:25 and `imm[4:1|11]` in bits 11:7.
pub fn b_imm(word: u64, v: i64) -> u64 {
    let v = v as u64;
    (word & 0x01ff_f07f)
        | ((v >> 12) & 1) << 31
        | ((v >> 5) & 0x3f) << 25
        | ((v >> 1) & 0xf) << 8
        | ((v >> 11) & 1) << 7
}

/// U-type: the value is the 20-bit field itself, as written by `lui rd, 1`.
pub fn u_imm(word: u64, v: i64) -> u64 {
    (word & 0xfff) | ((v as u64 & 0xf_ffff) << 12)
}

/// `%hi(sym)`: the upper 20 bits, biased so that the sign-extended `%lo`
/// added afterwards lands on the right address.
pub fn hi20(word: u64, v: i64) -> u64 {
    u_imm(word, ((v as u64).wrapping_add(0x800) >> 12) as i64)
}

/// `%lo(sym)` in an I-type field.
pub fn lo12_i(word: u64, v: i64) -> u64 {
    i_imm(word, v)
}

/// `%lo(sym)` in an S-type field.
pub fn lo12_s(word: u64, v: i64) -> u64 {
    s_imm(word, v)
}

/// J-type: `imm[20|10:1|11|19:12]` in bits 31:12.
pub fn j_imm(word: u64, v: i64) -> u64 {
    let v = v as u64;
    (word & 0xfff)
        | ((v >> 20) & 1) << 31
        | ((v >> 1) & 0x3ff) << 21
        | ((v >> 11) & 1) << 20
        | ((v >> 12) & 0xff) << 12
}

/// CJ-type: `imm[11|4|9:8|10|6|7|3:1|5]` in bits 12:2.
pub fn cj_imm(word: u64, v: i64) -> u64 {
    let v = v as u64;
    (word & 0xe003)
        | ((v >> 11) & 1) << 12
        | ((v >> 4) & 1) << 11
        | ((v >> 8) & 3) << 9
        | ((v >> 10) & 1) << 8
        | ((v >> 6) & 1) << 7
        | ((v >> 7) & 1) << 6
        | ((v >> 1) & 7) << 3
        | ((v >> 5) & 1) << 2
}

/// CB-type: `imm[8|4:3]` in bits 12:10 and `imm[7:6|2:1|5]` in bits 6:2.
pub fn cb_imm(word: u64, v: i64) -> u64 {
    let v = v as u64;
    (word & 0xe383)
        | ((v >> 8) & 1) << 12
        | ((v >> 3) & 3) << 10
        | ((v >> 6) & 3) << 5
        | ((v >> 1) & 3) << 3
        | ((v >> 5) & 1) << 2
}

/// An `auipc` followed by a `jalr`, patched as one field.
///
/// `call` splits a PC-relative address across two instruction words, and the
/// linker's `R_RISCV_CALL_PLT` covers both, so the pair is a single fixup here
/// too.
pub fn auipc_pair(word: u64, v: i64) -> u64 {
    let auipc = word & 0xffff_ffff;
    let second = word >> 32;
    let v = v as u64;
    let hi = (v.wrapping_add(0x800) >> 12) & 0xf_ffff;
    (auipc & 0xfff) | (hi << 12) | ((second & 0x000f_ffff) | ((v & 0xfff) << 20)) << 32
}

// ---- fixup kinds ----------------------------------------------------------

/// A 12-bit signed I-type immediate written as a plain number.
pub fn kind_i() -> FixupKind {
    FixupKind::data(4)
        .signed()
        .with_field(12, 1)
        .with_reloc(reloc::LO12_I)
        .scatter(i_imm)
}

pub fn kind_s() -> FixupKind {
    FixupKind::data(4)
        .signed()
        .with_field(12, 1)
        .with_reloc(reloc::LO12_S)
        .scatter(s_imm)
}

/// The 20-bit field of `lui`/`auipc` written as a plain number.
pub fn kind_u() -> FixupKind {
    FixupKind::data(4).with_field(20, 1).scatter(u_imm)
}

pub fn kind_hi20(pcrel: bool) -> FixupKind {
    let base = if pcrel {
        FixupKind::pcrel(4, 0).with_reloc(reloc::PCREL_HI20)
    } else {
        FixupKind::data(4).with_reloc(reloc::HI20)
    };
    base.scatter(hi20)
}

/// `%pcrel_lo(label)`, which names the `auipc` that carries the high half.
///
/// In relocatable output the linker pairs the two halves up, and the
/// relocation names the label itself: lld finds the `auipc` from the symbol's
/// value and ignores an addend, so the usual section-plus-offset would point
/// it at the start of the section. In a flat binary the core does the
/// pairing: the field receives the low bits of the `auipc`'s own PC-relative
/// value, not anything computed from the label's address.
pub fn kind_lo12(store: bool) -> FixupKind {
    let (reloc, f): (u32, fn(u64, i64) -> u64) = if store {
        (reloc::PCREL_LO12_S, lo12_s)
    } else {
        (reloc::PCREL_LO12_I, lo12_i)
    };
    FixupKind::data(4)
        .with_reloc(reloc)
        .scatter(f)
        .with_reloc_symbol(RelocSymbol::Symbol)
        .link(LinkValue::PairedLow)
}

/// The low half of an `auipc` pair a pseudo-instruction expanded to, such as
/// `la a0, sym` or `lw a0, sym`, in the word after the `auipc`.
///
/// The value is measured from the `auipc` four bytes back, so wherever the
/// high half resolves this one does too, and together they form the whole
/// offset. Where it cannot, the relocation names a label at the `auipc`, as
/// the psABI requires and as llvm-mc writes it; the expansion is its own
/// fragment, so the `auipc` starts it.
pub fn kind_pair_lo12(store: bool) -> FixupKind {
    let (reloc, f): (u32, fn(u64, i64) -> u64) = if store {
        (reloc::PCREL_LO12_S, lo12_s)
    } else {
        (reloc::PCREL_LO12_I, lo12_i)
    };
    FixupKind::pcrel(4, -4)
        .with_reloc(reloc)
        .scatter(f)
        .with_reloc_symbol(RelocSymbol::FragmentStart)
}

/// The `auipc` of `lga` (or `la` under `.option pic`), and of an explicit
/// `%got_pcrel_hi`, which addresses the symbol's GOT slot.
///
/// The slot is the linker's to place, so the field is never resolved here,
/// even for a label in the same section, and the relocation names the symbol
/// itself: a GOT entry belongs to a symbol, not to an offset into a section.
/// Both references also take a number, which asks the linker for the slot of
/// nothing at all and so is relocated against no symbol.
pub fn kind_got_hi20() -> FixupKind {
    FixupKind::data(4)
        .with_reloc(reloc::GOT_HI20)
        .scatter(hi20)
        .with_reloc_symbol(RelocSymbol::Symbol)
        .linker_only()
        .relocated_in_objects()
}

/// The load from the GOT slot that [`kind_got_hi20`] addressed, and the
/// second half of a thread-local pair, which reaches a slot the linker lays
/// out in the same way.
pub fn kind_got_lo12() -> FixupKind {
    kind_pair_lo12(false).linker_only()
}

// ---- thread-local access models -------------------------------------------

/// The 20-bit half of a thread-local sequence: `%tprel_hi`, the `auipc` of
/// `%tls_ie_pcrel_hi`, `%tls_gd_pcrel_hi` and `%tlsdesc_hi`.
///
/// Nothing here computes one. The value is an offset into a thread's block,
/// or the address of a GOT slot or descriptor holding one, which only the
/// linker lays out, and it may rewrite the whole sequence into another model;
/// so even a variable defined in this file is relocated. The relocation names
/// the variable, which the core does for a thread-local target of its own
/// accord.
pub fn kind_tls_hi20(reloc: u32) -> FixupKind {
    FixupKind::data(4)
        .with_reloc(reloc)
        .with_class(RelocClass::ThreadLocal)
        .scatter(hi20)
        .linker_only()
}

/// `%tprel_lo(sym)`, the low twelve bits of the same offset, in an I- or
/// S-type field.
pub fn kind_tprel_lo12(store: bool) -> FixupKind {
    let (reloc, f): (u32, fn(u64, i64) -> u64) = if store {
        (reloc::TPREL_LO12_S, lo12_s)
    } else {
        (reloc::TPREL_LO12_I, lo12_i)
    };
    FixupKind::data(4)
        .with_reloc(reloc)
        .with_class(RelocClass::ThreadLocal)
        .scatter(f)
        .linker_only()
}

/// `add rd, rs1, tp, %tprel_add(sym)`: the mark that tells the linker which
/// add of the thread pointer belongs to a local-exec sequence, so that it can
/// delete the whole sequence when it relaxes one.
///
/// The instruction has no field for it, and the relocation covers the four
/// bytes of the `add` without changing them. It still names the variable
/// rather than a label, and GNU as refuses one that is not thread-local, so
/// the width is the instruction's rather than zero: a fixup of no width is a
/// mark that makes no claim about what it names.
pub fn kind_tprel_add() -> FixupKind {
    FixupKind::data(4)
        .with_reloc(reloc::TPREL_ADD)
        .with_class(RelocClass::ThreadLocal)
        .scatter(keep_word)
        .linker_only()
}

/// `%tlsdesc_load_lo(label)` and `%tlsdesc_add_lo(label)`, the twelve-bit
/// halves of a descriptor sequence.
///
/// Like `%pcrel_lo`, each names the label on the `auipc` that carries
/// `%tlsdesc_hi`, which the linker looks up to find the descriptor; the
/// variable itself is named only by that `auipc`. Naming a label rather than
/// a variable is why the class is the plain one: a thread-local field is one
/// the core checks names a thread-local variable, and GNU as makes no such
/// check here.
pub fn kind_tlsdesc_lo12(reloc: u32) -> FixupKind {
    FixupKind::data(4)
        .with_reloc(reloc)
        .scatter(lo12_i)
        .with_reloc_symbol(RelocSymbol::Symbol)
        .linker_only()
}

/// `jalr rd, rs1, %tlsdesc_call(label)`, which marks the call through the
/// descriptor. The instruction has no field for it, so the mark covers no
/// bytes; it names the `auipc`'s label like the two above.
pub fn kind_tlsdesc_call() -> FixupKind {
    FixupKind::data(0)
        .with_reloc(reloc::TLSDESC_CALL)
        .with_class(RelocClass::ThreadLocal)
        .with_reloc_symbol(RelocSymbol::Symbol)
        .linker_only()
}

fn keep_word(w: u64, _: i64) -> u64 {
    w
}

/// `%lo(sym)`, which takes the low 12 bits of an absolute address and so has
/// no range to check.
pub fn kind_abs_lo12(store: bool) -> FixupKind {
    if store {
        FixupKind::data(4).with_reloc(reloc::LO12_S).scatter(lo12_s)
    } else {
        FixupKind::data(4).with_reloc(reloc::LO12_I).scatter(lo12_i)
    }
}

pub fn kind_branch() -> FixupKind {
    FixupKind::pcrel(4, 0)
        .with_field(13, 2)
        .with_reloc(reloc::BRANCH)
        .scatter(b_imm)
}

pub fn kind_jal() -> FixupKind {
    FixupKind::pcrel(4, 0)
        .with_field(21, 2)
        .with_reloc(reloc::JAL)
        .scatter(j_imm)
}

pub fn kind_cb() -> FixupKind {
    FixupKind::pcrel(2, 0)
        .with_field(9, 2)
        .with_reloc(reloc::RVC_BRANCH)
        .scatter(cb_imm)
}

pub fn kind_cj() -> FixupKind {
    FixupKind::pcrel(2, 0)
        .with_field(12, 2)
        .with_reloc(reloc::RVC_JUMP)
        .scatter(cj_imm)
}

/// `R_RISCV_ALIGN`, which hands an alignment in a relaxable code section to
/// the linker: the mark sits on the first byte of the padding and its value is
/// how many of those bytes the linker may delete.
///
/// It names no symbol and fills no field, so the value is the addend alone,
/// which is what `relocated_in_objects` leaves behind. Nothing can resolve it
/// here: only the linker knows what the section will finally be, which is the
/// whole point of writing it.
pub fn kind_align() -> FixupKind {
    FixupKind::data(0)
        .with_reloc(reloc::ALIGN)
        .linker_only()
        .relocated_in_objects()
}

/// The field a call frame address advance goes in, where a relaxing linker
/// is to work the advance out; see `Architecture::cfa_advance_field`.
///
/// `DW_CFA_advance_loc` packs six bits of advance into the byte that carries
/// its opcode, so the relocation there replaces those six bits and leaves the
/// other two alone. The wider forms have an operand of their own, and take
/// the whole field.
pub fn kind_cfa_advance(size: u8, bits: u8) -> Option<FixupKind> {
    Some(match (size, bits) {
        (1, 6) => FixupKind::data(1)
            .with_field(6, 1)
            .with_reloc(reloc::SET6)
            .scatter(|word, value| (word & !0x3f) | (value as u64 & 0x3f)),
        (1, 8) => FixupKind::data(1).with_reloc(reloc::SET8),
        (2, 16) => FixupKind::data(2).with_reloc(reloc::SET16),
        (4, 32) => FixupKind::data(4).with_reloc(reloc::SET32),
        _ => return None,
    })
}

/// The `auipc`/`jalr` pair of `call`, patched as one eight-byte field.
///
/// llvm-mc 22 writes `R_RISCV_CALL_PLT` whether or not the source said
/// `@plt`; the psABI has deprecated the plain `R_RISCV_CALL`.
pub fn kind_call() -> FixupKind {
    FixupKind::pcrel(8, 0)
        .with_field(32, 1)
        .with_reloc(reloc::CALL_PLT)
        .scatter(auipc_pair)
}

// ---- assembling a fragment ------------------------------------------------

/// An instruction word, plus the immediate that is not known yet.
#[derive(Clone, Copy)]
pub struct Insn {
    pub word: u32,
    /// Two bytes rather than four.
    pub compressed: bool,
    pub fix: Option<Fix>,
}

#[derive(Clone, Copy)]
pub struct Fix {
    pub expr: ExprRef,
    pub kind: FixupKind,
    pub span: Span,
}

impl Insn {
    pub fn full(word: u32) -> Insn {
        Insn {
            word,
            compressed: false,
            fix: None,
        }
    }

    pub fn short(word: u16) -> Insn {
        Insn {
            word: word as u32,
            compressed: true,
            fix: None,
        }
    }

    pub fn with_fix(mut self, expr: ExprRef, kind: FixupKind, span: Span) -> Insn {
        self.fix = Some(Fix { expr, kind, span });
        self
    }
}

/// A sequence of instructions being turned into one fragment.
#[derive(Default)]
pub struct Buf {
    bytes: Vec<u8>,
    fixups: Vec<Fixup>,
}

impl Buf {
    pub fn push(&mut self, insn: Insn) {
        let at = self.bytes.len() as u32;
        if insn.compressed {
            self.bytes
                .extend_from_slice(&(insn.word as u16).to_le_bytes());
        } else {
            self.bytes.extend_from_slice(&insn.word.to_le_bytes());
        }
        if let Some(f) = insn.fix {
            self.fixups.push(Fixup {
                offset: at,
                expr: f.expr,
                kind: f.kind,
                span: f.span,
            });
        }
    }

    /// Two words covered by a single fixup, as `call` needs.
    pub fn push_pair(&mut self, first: u32, second: u32, fix: Fix) {
        let at = self.bytes.len() as u32;
        self.bytes.extend_from_slice(&first.to_le_bytes());
        self.bytes.extend_from_slice(&second.to_le_bytes());
        self.fixups.push(Fixup {
            offset: at,
            expr: fix.expr,
            kind: fix.kind,
            span: fix.span,
        });
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn finish(self) -> Variant {
        Variant {
            bytes: self.bytes,
            fixups: self.fixups,
        }
    }
}

/// `addi x0, x0, 0`, the canonical no-op.
pub const NOP: u32 = 0x0000_0013;

pub fn nop_bytes(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut left = len;
    // An odd byte cannot hold any instruction, so it goes first as zero and
    // leaves the rest of the run aligned.
    if left % 2 == 1 {
        out.push(0);
        left -= 1;
    }
    if left % 4 == 2 {
        out.extend_from_slice(&super::compress::C_NOP.to_le_bytes());
        left -= 2;
    }
    while left >= 4 {
        out.extend_from_slice(&NOP.to_le_bytes());
        left -= 4;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_immediates_scatter_and_come_back() {
        // `beq a0, a1, .+8`, from the opcode word with a zero displacement.
        let w = b_imm(0x00b5_0063, 8);
        assert_eq!(w as u32, 0x00b5_0463);
        // A negative displacement must not disturb the register fields.
        let w = b_imm(0x00b5_0063, -8);
        assert_eq!(w as u32, 0xfeb5_0ce3);
    }

    #[test]
    fn hi_lo_split_round_trips_through_sign_extension() {
        // 0x123456789's low half is negative as a 12-bit value, so `%hi` has
        // to be biased upwards to compensate.
        let lui = hi20(0x0000_0537, 0x1234_5800) as u32;
        assert_eq!(lui >> 12, 0x12346);
        let addi = lo12_i(0x0005_0513, 0x1234_5800) as u32;
        assert_eq!((addi as i32) >> 20, 0x800 - 0x1000);
    }

    #[test]
    fn padding_uses_real_no_ops() {
        assert_eq!(nop_bytes(0), Vec::<u8>::new());
        assert_eq!(nop_bytes(2), vec![0x01, 0x00]);
        assert_eq!(nop_bytes(4), vec![0x13, 0x00, 0x00, 0x00]);
        assert_eq!(nop_bytes(6), vec![0x01, 0x00, 0x13, 0x00, 0x00, 0x00]);
        assert_eq!(nop_bytes(3), vec![0x00, 0x01, 0x00]);
    }
}
