//! Bit-level helpers: instruction words, fixups, and the logical-immediate
//! bitmask encoding.

use super::reloc;
use crate::arch::AsmCtx;
use crate::expr::ExprRef;
use crate::reloc::RelocClass;
use crate::section::{Fixup, FixupKind, LinkValue, Variant};
use crate::source::Span;

/// One 32-bit instruction word, little-endian, with no fixup.
pub fn word(w: u32) -> Variant {
    Variant::new(w.to_le_bytes().to_vec())
}

/// An instruction word with one fixup covering the whole word.
pub fn word_fixup(w: u32, expr: ExprRef, kind: FixupKind, span: Span) -> Variant {
    Variant {
        bytes: w.to_le_bytes().to_vec(),
        fixups: vec![Fixup {
            offset: 0,
            expr,
            kind,
            span,
        }],
    }
}

/// Places `value` into the `bits`-wide field starting at `lsb`.
pub const fn field(value: u32, lsb: u32, bits: u32) -> u32 {
    let mask = if bits >= 32 {
        u32::MAX
    } else {
        (1 << bits) - 1
    };
    (value & mask) << lsb
}

// ---- branch and PC-relative fixups ----------------------------------------
//
// Every scatter function keeps the bits outside its field, because the word
// handed to it is the opcode this backend already emitted. `value_bits`
// counts the bits of *value*, which is two more than the encoded field
// wherever the field counts instructions rather than bytes.

fn scatter_imm26(w: u64, v: i64) -> u64 {
    (w & !0x03ff_ffff) | (((v >> 2) as u64) & 0x03ff_ffff)
}

fn scatter_imm19(w: u64, v: i64) -> u64 {
    (w & !(0x7_ffff << 5)) | ((((v >> 2) as u64) & 0x7_ffff) << 5)
}

fn scatter_imm14(w: u64, v: i64) -> u64 {
    (w & !(0x3fff << 5)) | ((((v >> 2) as u64) & 0x3fff) << 5)
}

fn scatter_imm9(w: u64, v: i64) -> u64 {
    (w & !(0x1ff << 5)) | ((((v >> 2) as u64) & 0x1ff) << 5)
}

/// `adr`: a byte offset split into a 2-bit low part and a 19-bit high part.
fn scatter_adr(w: u64, v: i64) -> u64 {
    let v = v as u64;
    (w & !0x60ff_ffe0) | ((v & 3) << 29) | (((v >> 2) & 0x7_ffff) << 5)
}

/// `b` / `bl`: +/-128MB, in whole instructions.
pub fn fixup_b() -> FixupKind {
    FixupKind::pcrel(4, 0)
        .with_field(28, 4)
        .with_reloc(reloc::JUMP26)
        .with_class(RelocClass::Branch)
        .scatter(scatter_imm26)
}

pub fn fixup_call() -> FixupKind {
    fixup_b().with_reloc(reloc::CALL26)
}

/// `b.<cond>`, `cbz`, `cbnz` and the literal loads: +/-1MB.
pub fn fixup_b19() -> FixupKind {
    FixupKind::pcrel(4, 0)
        .with_field(21, 4)
        .with_reloc(reloc::CONDBR19)
        .scatter(scatter_imm19)
}

pub fn fixup_ld_lit() -> FixupKind {
    FixupKind::pcrel(4, 0)
        .with_field(21, 4)
        .with_reloc(reloc::LD_PREL_LO19)
        .scatter(scatter_imm19)
}

/// `ldr x0, :got:sym`: the symbol's GOT slot, reached from the instruction.
/// Only the linker knows where that is, so a flat image refuses it.
pub fn fixup_got_ld_lit() -> FixupKind {
    fixup_ld_lit()
        .with_reloc(reloc::GOT_LD_PREL19)
        .with_class(RelocClass::Got)
        .link(LinkValue::LinkerOnly("a GOT entry"))
}

/// FEAT_CMPBR's `cb<cc>`: +/-1KB, the narrowest branch field A64 has.
///
/// It has no relocation, so a target this assembler cannot resolve itself is
/// an error. Neither reference can relocate one either: llvm-mc refuses the
/// line, and GNU as writes `R_AARCH64_NONE`, which leaves the branch
/// pointing at itself for want of a relocation type the psABI has not
/// defined.
pub fn fixup_b9() -> FixupKind {
    FixupKind::pcrel(4, 0)
        .with_field(11, 4)
        .scatter(scatter_imm9)
}

/// `tbz` / `tbnz`: +/-32KB.
pub fn fixup_b14() -> FixupKind {
    FixupKind::pcrel(4, 0)
        .with_field(16, 4)
        .with_reloc(reloc::TSTBR14)
        .scatter(scatter_imm14)
}

/// `adr`: +/-1MB, byte-granular.
pub fn fixup_adr() -> FixupKind {
    FixupKind::pcrel(4, 0)
        .with_field(21, 1)
        .with_reloc(reloc::ADR_PREL_LO21)
        .scatter(scatter_adr)
}

/// `adrp`: the 21-bit field counts 4KB pages between `page(PC)` and
/// `page(target)`.
///
/// The fixup is deliberately *not* marked PC-relative. Page arithmetic needs
/// both addresses, not their difference, so an `adrp` must always reach the
/// linker, which is also what GNU as and llvm-mc do even for a target in the
/// same section. Leaving the fixup absolute makes the core give up on any
/// section-relative symbol and emit `R_AARCH64_ADR_PREL_PG_HI21`, which is
/// the correct output; in a flat image, where both addresses are known, the
/// core does the page arithmetic the linker would have.
pub fn fixup_adrp() -> FixupKind {
    FixupKind::data(4)
        .with_field(33, 1)
        .with_reloc(reloc::ADR_PREL_PG_HI21)
        .with_class(RelocClass::Page)
        .link(LinkValue::Page(12))
        .scatter(scatter_adrp)
}

fn scatter_adrp(w: u64, v: i64) -> u64 {
    let pages = (v >> 12) as u64;
    (w & !0x60ff_ffe0) | ((pages & 3) << 29) | (((pages >> 2) & 0x7_ffff) << 5)
}

/// `adrp x0, :got:sym`: the page of the symbol's GOT slot. Only the linker
/// knows where that is, so a flat image refuses it.
pub fn fixup_got_page() -> FixupKind {
    fixup_adrp()
        .with_reloc(reloc::ADR_GOT_PAGE)
        .with_class(RelocClass::GotPage)
        .link(LinkValue::LinkerOnly("a GOT entry"))
}

// ---- move-wide groups -------------------------------------------------------

fn scatter_movw<const GROUP: u32>(w: u64, v: i64) -> u64 {
    (w & !(0xffff << 5)) | ((((v >> (16 * GROUP)) as u64) & 0xffff) << 5)
}

/// A signed group, where GNU as settles a value it can work out by choosing
/// the instruction: `movz` holds a value that is not negative and `movn`
/// holds the inverse of one that is, whichever of the two was written. The
/// operators that reach here are refused on `movk`, so the opcode is always
/// one of those two.
fn scatter_movw_signed<const GROUP: u32>(w: u64, v: i64) -> u64 {
    let g = v >> (16 * GROUP);
    let (opc, imm) = if g < 0 { (0, !g as u64) } else { (2, g as u64) };
    (w & !(0xffff << 5) & !(3 << 29)) | (opc << 29) | ((imm & 0xffff) << 5)
}

/// `movz x0, :abs_g1_nc:sym` and its relatives: the 16-bit field holds one
/// group of the symbol's address.
///
/// The fixup is absolute even for the `:prel_gN:` operators, as GNU as's is.
/// Nothing subtracts the instruction's address at assembly time: the
/// relocation is what makes the value PC-relative, so a label in the fixup's
/// own section still reaches the linker while a constant is taken as
/// written. [`LinkValue::Page`] with byte-sized pages is that subtraction,
/// and only a flat image, which has no linker to do it, ever asks for it.
///
/// The range is stated as the whole value rather than the group, since
/// "nothing above group `n`" is exactly "fits in `16 * (n + 1)` bits", and
/// the group is taken as the field is written.
///
/// A thread-local group is left to the linker whatever it refers to; see
/// [`fixup_tls`].
pub fn fixup_movw(g: super::operand::MovwGroup) -> FixupKind {
    use super::operand::MovwCheck;
    if g.tls {
        return fixup_tls(g.reloc);
    }
    let bits = 16 * (g.group + 1);
    let plain: fn(u64, i64) -> u64 = match g.group {
        0 => scatter_movw::<0>,
        1 => scatter_movw::<1>,
        2 => scatter_movw::<2>,
        _ => scatter_movw::<3>,
    };
    let kind = FixupKind::data(4)
        .with_reloc(g.reloc)
        .with_class(RelocClass::AddressGroup);
    let kind = match g.check {
        MovwCheck::None => kind.with_field(64, 1).scatter(plain),
        MovwCheck::Unsigned => kind
            .with_field(bits, 1)
            .with_limits(0, (1i64 << bits) - 1)
            .scatter(plain),
        MovwCheck::Signed => kind.with_field(bits, 1).signed().scatter(match g.group {
            0 => scatter_movw_signed::<0>,
            1 => scatter_movw_signed::<1>,
            _ => scatter_movw_signed::<2>,
        }),
    };
    if g.prel {
        kind.link(LinkValue::Page(0))
    } else {
        kind
    }
}

// ---- `:lo12:` fields --------------------------------------------------------
//
// The low twelve bits of an address, completing what an `adrp` started. Like
// `adrp` these are absolute fixups so that a symbol in a relocatable section
// always reaches the linker; an absolute value (a `.set` constant, or flat
// output) is resolved here instead, keeping only the low bits as the linker
// would.

fn scatter_imm12(w: u64, v: i64) -> u64 {
    (w & !(0xfff << 10)) | (((v as u64) & 0xfff) << 10)
}

/// A load/store offset field counts in units of the access size, so the low
/// twelve bits are scaled down before they go in.
fn scatter_imm12_scaled<const SCALE: u32>(w: u64, v: i64) -> u64 {
    (w & !(0xfff << 10)) | ((((v as u64) & 0xfff) >> SCALE) << 10)
}

/// `add x0, x0, :lo12:sym`.
pub fn fixup_lo12_add() -> FixupKind {
    FixupKind::data(4)
        .with_field(64, 1)
        .with_reloc(reloc::ADD_ABS_LO12_NC)
        .with_class(RelocClass::PageOff)
        .scatter(scatter_imm12)
}

/// `ldr x0, [x0, :lo12:sym]`, for an access of `1 << scale` bytes. An
/// absolute value must be a multiple of the access size, or its low bits
/// would not fit the scaled field.
pub fn fixup_lo12_ldst(scale: u32) -> FixupKind {
    let (reloc, f): (u32, fn(u64, i64) -> u64) = match scale {
        0 => (reloc::LDST8_ABS_LO12_NC, scatter_imm12_scaled::<0>),
        1 => (reloc::LDST16_ABS_LO12_NC, scatter_imm12_scaled::<1>),
        2 => (reloc::LDST32_ABS_LO12_NC, scatter_imm12_scaled::<2>),
        3 => (reloc::LDST64_ABS_LO12_NC, scatter_imm12_scaled::<3>),
        _ => (reloc::LDST128_ABS_LO12_NC, scatter_imm12_scaled::<4>),
    };
    FixupKind::data(4)
        .with_field(64, 1 << scale.min(4))
        .with_reloc(reloc)
        .with_class(RelocClass::PageOff)
        .scatter(f)
}

/// `ldr x0, [x0, :got_lo12:sym]`, or with `scale` 2 Darwin's
/// `ldr w0, [x0, sym@GOTPAGEOFF]`.
pub fn fixup_got_lo12(scale: u32) -> FixupKind {
    fixup_lo12_ldst(scale)
        .with_reloc(reloc::LD64_GOT_LO12_NC)
        .with_class(RelocClass::GotPageOff)
        .link(LinkValue::LinkerOnly("a GOT entry"))
}

// ---- thread-local access models ---------------------------------------------

/// The field of a thread-local operator: `:tprel_lo12:`, `:tlsdesc:`,
/// `:gottprel_g1:` and the rest, each with the relocation GNU as writes for
/// the instruction it is on.
///
/// Nothing here computes one. The value is an offset into a thread's block,
/// or the address of a GOT slot or descriptor holding one, which only the
/// linker lays out; and the linker may rewrite the whole sequence into
/// another model, so even a variable defined in this file is relocated.
/// GNU as leaves the field as it was written, `movz` staying `movz`, which
/// the scatter function does too, should anything ever write one.
pub fn fixup_tls(reloc: u32) -> FixupKind {
    FixupKind::data(4)
        .with_field(64, 1)
        .with_reloc(reloc)
        .with_class(RelocClass::ThreadLocal)
        .linker_only()
        .scatter(keep_word)
}

fn keep_word(w: u64, _: i64) -> u64 {
    w
}

/// A mark on an instruction of a descriptor sequence, `.tlsdesccall v` on
/// the `blr` after it: a relocation covering no bytes, which tells the
/// linker what the instruction is for when it rewrites the sequence.
pub fn fixup_tls_mark(reloc: u32) -> FixupKind {
    FixupKind::data(0)
        .with_reloc(reloc)
        .with_class(RelocClass::ThreadLocal)
        .linker_only()
}

// ---- the logical (bitmask) immediate ---------------------------------------

/// Encodes a bitmask immediate as `N:immr:imms`, or `None` if the value is not
/// one.
///
/// The `and`/`orr`/`eor` immediate forms cannot hold an arbitrary constant.
/// What they hold is a *repeating bit pattern*: pick an element width `e` in
/// {2,4,8,16,32,64}, put a run of `n` ones at the bottom of the element,
/// rotate the element right by `immr`, and replicate it to fill the register.
/// That covers 5334 distinct 64-bit values — every mask you would actually
/// write, and nothing else.
///
/// The three encoded fields are unusual because `imms` has to carry both the
/// element width and the run length. It does that by prefixing the run length
/// with a unary marker: `imms = 0b11110n` for e=2, `0b1110nn` for e=4,
/// `0b110nnn` for e=8, and so on down to `0b0nnnnn` for e=32. e=64 needs a
/// sixth pattern and there are no bits left, so it borrows `N`: `N=1` means
/// e=64 and `imms` is the run length outright.
///
/// This is a port of LLVM's `processLogicalImmediate`, which is the clearest
/// statement of the inverse mapping.
pub fn logical_imm(value: u64, reg_bits: u32) -> Option<(u32, u32, u32)> {
    let mut imm = value;
    // A 32-bit operation encodes as a 64-bit pattern with e <= 32, so anything
    // that does not fit in 32 bits, and the all-ones pattern, are out.
    if imm == 0 || imm == u64::MAX {
        return None;
    }
    if reg_bits != 64 && (imm >> reg_bits != 0 || imm == u64::MAX >> (64 - reg_bits)) {
        return None;
    }

    // The element size is the smallest period the value repeats with.
    let mut size = reg_bits;
    loop {
        size /= 2;
        let mask = (1u64 << size) - 1;
        if (imm & mask) != ((imm >> size) & mask) {
            size *= 2;
            break;
        }
        if size <= 2 {
            break;
        }
    }

    // Within one element the pattern must be a run of ones, possibly rotated
    // so that it wraps around the top.
    let mask = u64::MAX >> (64 - size);
    imm &= mask;
    let (rotation, run) = if is_shifted_mask(imm) {
        let i = imm.trailing_zeros();
        (i, (imm >> i).trailing_ones())
    } else {
        // A wrapped run: ones at both ends. Filling the bits above the element
        // turns it into a run of *zeros* in the middle, which is a shifted
        // mask when inverted.
        imm |= !mask;
        if !is_shifted_mask(!imm) {
            return None;
        }
        let leading_ones = (!imm).leading_zeros();
        (
            64 - leading_ones,
            leading_ones + imm.trailing_ones() - (64 - size),
        )
    };

    let immr = (size - rotation) % size;
    // `!(size-1) << 1` is the unary width marker; the run length, less one,
    // goes in the bits below it.
    let nimms = (!(size - 1) << 1) | (run - 1);
    let n = ((nimms >> 6) & 1) ^ 1;
    Some((n, immr & 0x3f, nimms & 0x3f))
}

fn is_shifted_mask(v: u64) -> bool {
    v != 0 && is_mask((v - 1) | v)
}

fn is_mask(v: u64) -> bool {
    v != 0 && v.wrapping_add(1) & v == 0
}

// ---- shared operand checks -------------------------------------------------

/// A constant that must be known at assembly time, with a range check.
pub fn const_in_range(
    cx: &mut AsmCtx<'_>,
    e: ExprRef,
    lo: i64,
    hi: i64,
    what: &str,
) -> Option<i64> {
    let span = cx.exprs.span(e);
    match cx.constant(e) {
        Some(v) if v >= lo && v <= hi => Some(v),
        Some(v) => {
            cx.error(span, format!("{what} must be {lo}..={hi}, but is {v}"));
            None
        }
        None => {
            cx.error(span, format!("{what} must be a constant"));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Repacks the three fields the way the instruction word carries them, so
    /// a case can be compared against a whole encoding.
    fn packed(v: u64, bits: u32) -> Option<u32> {
        let (n, immr, imms) = logical_imm(v, bits)?;
        Some((n << 12) | (immr << 6) | imms)
    }

    #[test]
    fn simple_low_runs() {
        // 0xff in a 64-bit register: N=1 (e=64), no rotation, 8 ones.
        assert_eq!(packed(0xff, 64), Some((1 << 12) | 7));
        // The same value in a 32-bit register: e=32, so N=0 and the width
        // marker in imms is 0b0.....
        assert_eq!(packed(0xff, 32), Some(7));
    }

    #[test]
    fn repeating_patterns_pick_the_smallest_element() {
        // 0x5555...: e=2, one bit set per element.
        let (n, immr, imms) = logical_imm(0x5555_5555_5555_5555, 64).unwrap();
        assert_eq!((n, immr), (0, 0));
        assert_eq!(imms, 0b111100);
        // 0xffff0000ffff0000: e=32, 16 ones rotated up by 16.
        let (n, immr, imms) = logical_imm(0xffff_0000_ffff_0000, 64).unwrap();
        assert_eq!((n, immr, imms), (0, 16, 0b001111));
    }

    #[test]
    fn wrapped_runs_encode_as_a_rotation() {
        // 0xf000_0000_0000_000f is a run of eight ones rotated right by four.
        let (n, immr, imms) = logical_imm(0xf000_0000_0000_000f, 64).unwrap();
        assert_eq!((n, immr, imms), (1, 4, 7));
    }

    #[test]
    fn rejects_values_that_are_not_replicated_runs() {
        assert_eq!(logical_imm(0, 64), None);
        assert_eq!(logical_imm(u64::MAX, 64), None);
        assert_eq!(logical_imm(0xffff_ffff, 32), None);
        assert_eq!(logical_imm(0b1011, 64), None);
        assert_eq!(
            logical_imm(0x1_0000_0000, 32),
            None,
            "wider than the register"
        );
    }

    #[test]
    fn every_encodable_value_round_trips() {
        // Enumerate the encoding space and check that decoding each triple and
        // re-encoding the result reproduces it. This is the only way to be
        // sure the inverse mapping has no holes.
        let mut seen = 0;
        for n in 0..2u32 {
            for immr in 0..64u32 {
                for imms in 0..64u32 {
                    let Some(v) = decode(n, immr, imms) else {
                        continue;
                    };
                    seen += 1;
                    assert_eq!(
                        logical_imm(v, 64),
                        Some((n, immr, imms)),
                        "n={n} immr={immr} imms={imms} value={v:#x}"
                    );
                }
            }
        }
        // The ARM ARM counts 5334 encodable 64-bit bitmask immediates.
        assert_eq!(seen, 5334);
    }

    /// The decode side of the bitmask scheme, straight out of the ARM ARM's
    /// `DecodeBitMasks`. Used only by the round-trip test above.
    fn decode(n: u32, immr: u32, imms: u32) -> Option<u64> {
        // The element size is the position of the top set bit of `N:~imms`;
        // no set bit at all is a reserved encoding.
        let combined = (n << 6) | (!imms & 0x3f);
        let len = 31u32.checked_sub(combined.leading_zeros())?;
        if len < 1 {
            return None;
        }
        let size = 1u32 << len;
        let levels = size - 1;
        // A run as long as the element would mean all ones, which is what the
        // N:imms encoding cannot represent.
        if imms & levels == levels {
            return None;
        }
        // Bits of `immr` above the element width are ignored by the hardware,
        // so several encodings decode to the same value; only the canonical
        // one is a round-trip candidate.
        if immr & !levels != 0 {
            return None;
        }
        let run = (imms & levels) + 1;
        let rot = immr & levels;
        let mask = u64::MAX >> (64 - size);
        let elem = (1u64 << run) - 1;
        // Rotate right within the element, not within 64 bits.
        let elem = ((elem >> rot) | (elem << ((size - rot) % size))) & mask;
        let mut v = 0u64;
        let mut i = 0;
        while i < 64 {
            v |= elem << i;
            i += size;
        }
        Some(v)
    }
}
