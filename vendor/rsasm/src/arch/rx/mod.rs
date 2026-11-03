//! Renesas RX. `EM_RX` (173).
//!
//! RX is a 32-bit CISC microcontroller with instructions from one to eight
//! bytes, little-endian throughout. This backend implements the RXv1
//! instruction set in GNU as syntax — `mov.l #1, r1`, `add 4[r1].w, r2` —
//! and checks every encoding against `rx-elf-as` from binutils 2.47.
//!
//! - [`reg`]: register, flag and condition names, and operand sizes
//! - [`operand`]: the operand grammar
//! - [`encode`]: bit fields, displacements, and the value-dependent choice of
//!   immediate width that most of RX's size classes come from
//! - [`branch`]: branches and their relaxation, including GNU as's synthetic
//!   long conditional branches
//! - [`insn`]: the instruction table
//! - [`reloc`]: `R_RX_*` numbers
//!
//! # Differences from GNU as that are known
//!
//! GNU as for RX is not a plain assembler: it relaxes branches and symbolic
//! immediates itself, and with `-relax` also leaves `R_RX_RH_RELAX` markers so
//! the linker can shrink them further. It does neither by default, which is
//! how the differential corpus runs it. The details that do not carry over:
//!
//! - GNU as writes the symbol's value into the field of a relocation against
//!   a symbol defined in the same file; rsasm leaves the field zero. The
//!   linker overwrites it either way.
//! - An unresolved `beq`/`bne` or other conditional branch is given the
//!   synthetic long form rather than GNU as's 16- or 8-bit one.
//! - GNU as picks a far conditional's `bra.w` pair by the displacement from
//!   the conditional, not from the `bra.w`, so a target 32,769 or 32,770
//!   bytes back gets a 16-bit field that silently wraps and branches forward.
//!   rsasm takes the 6-byte `bra.a` pair there, which reaches. (Its other
//!   relaxation rules, shrinking included, are followed; see
//!   [`Relaxation::Shrinking`](crate::arch::Relaxation::Shrinking).)
//! - A displacement must be a constant or a difference of labels, as in GNU
//!   as, but its `%gp()` exception is not supported.
//! - What needs one of GNU as's relocation expressions, a stack of `R_RX_SYM`
//!   and `R_RX_OP*` entries, is refused: a difference of symbols that are not
//!   in one section, such as `.long ext - .`, and `sub #sym`, which stores the
//!   symbol negated. RX has no single relocation for either; even `.long ext
//!   - .` has no 32-bit PC-relative one to stand in for the stack.
//! - A bare number as a branch target (`bra 5`) is an address here; GNU as
//!   sizes the branch as if the number were the displacement.
//! - Code goes in whatever section is current, `.text` by default, where GNU
//!   as uses the Renesas name `P`.
//!
//! # CC-RX source
//!
//! With `-d ccrx` the operands take the two things Renesas CC-RX adds
//! (R20UT3248EJ0115 chapter 5): bit length specifiers, `#imm:8` and
//! `dsp:16[r1]`, and the substitute register names `__PID_R0`-`__PID_R15`.
//! CC-RX uses the width a specifier names even where a shorter form fits,
//! while this backend always assembles the shortest, as GNU as does; so a
//! specifier is accepted only where it names the width of that form, and
//! refused otherwise. See `insn::check_bit_lengths`. For a branch to a
//! target that is not resolved in the file, CC-RX keeps a conditional branch
//! at 8 or 16 bits as GNU as does, where this backend gives the synthetic
//! long form noted above.
//!
//! Only RXv1 is implemented, which is what GNU as accepts without `-mcpu`:
//! the RXv2/RXv3 instructions (`movco`, `emaca`, `save`, the double-precision
//! set, three-operand `xor`, register forms of `stz`/`stnz`) are refused with
//! a diagnostic that says so.

pub mod branch;
pub mod encode;
pub mod insn;
pub mod operand;
pub mod reg;
pub mod reloc;

use crate::arch::{ArchState, Architecture, AsmCtx, CommentSyntax, Endian, InsnRequest, Syntax};
use crate::section::Variant;

pub const NAMES: &[&str] = &["rx"];

pub fn lookup(name: &str) -> Option<Box<dyn Architecture>> {
    match name {
        "rx" | "rxv1" | "rx600" => Some(Box::new(Rx)),
        _ => None,
    }
}

pub struct Rx;

impl Architecture for Rx {
    fn name(&self) -> &'static str {
        "rx"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["rxv1", "rx600"]
    }

    /// Code is always little-endian. GNU as can make *data* big-endian with
    /// `-mbig-endian-data`, which this backend does not model.
    fn endian(&self) -> Endian {
        Endian::Little
    }

    fn pointer_bytes(&self, _state: &ArchState) -> u8 {
        4
    }

    fn initial_state(&self) -> ArchState {
        ArchState {
            bits: 32,
            syntax: Syntax::Att,
            features: 0,
            cpu_features: [0; 3],
            intel_register_prefix: false,
            used: 0,
            private: 0,
        }
    }

    /// There is no second RX operand syntax to switch to within GNU as.
    fn supports_syntax(&self, syntax: Syntax) -> bool {
        syntax == Syntax::Att
    }

    fn elf_machine(&self) -> u16 {
        173
    }

    /// `rx_relax_frag` in GNU as re-picks each branch and immediate size on
    /// every pass, so a `bne` that was too close for `.s` at first takes `.s`
    /// once the code it jumps over has grown.
    fn relaxation(&self) -> crate::arch::Relaxation {
        crate::arch::Relaxation::Shrinking
    }

    fn pcrel_number_is_address(&self) -> bool {
        true
    }

    /// `E_FLAG_RX_ABI`, which GNU as sets unless told to use the old ABI.
    fn elf_flags(&self, _state: &ArchState) -> u32 {
        0x8
    }

    fn pads_section_tail(&self, _flags: &crate::section::SectionFlags) -> bool {
        true
    }

    /// `;` comments anywhere; `#` is the immediate prefix, so it starts a
    /// comment only at the beginning of a line, as GNU as for RX has it.
    fn comments(&self) -> CommentSyntax {
        CommentSyntax {
            anywhere: &[";"],
            line_start: &["#"],
        }
    }

    /// GNU as for RX separates statements with `!`, since `;` is taken.
    fn tune_lexer(&self, cfg: &mut crate::lexer::LexConfig) {
        cfg.stmt_sep = vec!['!'];
    }

    /// `.word` is 32 bits in GNU as for RX, like `.int` and `.long`.
    fn word_bytes(&self) -> u8 {
        4
    }

    /// GNU as's `md_assemble` makes the row after writing the instruction,
    /// at the end less `idx`, which by then holds the byte offset of the
    /// last relocated field, or the length of an instruction without one.
    /// So a row is at the start of an instruction with constant operands,
    /// and near the end of any other, a relaxed branch's grown end included.
    /// The offsets come from the smallest encoding, the form GNU as parses.
    fn dwarf_row_back(&self, smallest: &Variant) -> Option<u32> {
        smallest.fixups.last().map(|f| f.offset)
    }

    /// GNU as leaves a local label's value in every relocated field, a
    /// PC-relative one included (checked: `.long lab+4`, `bsr lab` across
    /// sections).
    fn local_value_in_field(&self, _reloc: u32) -> bool {
        true
    }

    fn data_reloc(&self, size: u8, pcrel: bool) -> Option<u32> {
        reloc::data(size, pcrel)
    }

    fn nop_fill(&self, _state: &ArchState, len: u64) -> Vec<u8> {
        nop_fill(len as usize)
    }

    fn assemble(&self, cx: &mut AsmCtx<'_>, insn: &InsnRequest<'_>) -> Option<Vec<Variant>> {
        insn::assemble(cx, insn)
    }
}

/// Alignment padding, as GNU as writes it.
///
/// Up to seven bytes is a single instruction that does nothing, each taking
/// one clock cycle: `nop`, `mov.l r0, r0`, `max r0, r0`, and `mul #1, r0` or
/// `max #0x80000000, r0` with an immediate of the right length. Longer
/// padding is a `bra.b` over itself, which GNU as repeats as a two-byte
/// pattern. With an odd length GNU as drops the leftover byte, shifting
/// everything after it (visible as a stray zero at the end of the section);
/// here it is a zero byte in place, which the `bra.b` jumps over. Past 127
/// bytes, where `bra.b` cannot reach and GNU as's pattern jumps backwards,
/// it is a `bra.w` followed by the same dead zeroes.
fn nop_fill(len: usize) -> Vec<u8> {
    const NOPS: [&[u8]; 8] = [
        &[],
        &[0x03],
        &[0xef, 0x00],
        &[0xfc, 0x13, 0x00],
        &[0x76, 0x10, 0x01, 0x00],
        &[0x77, 0x10, 0x01, 0x00, 0x00],
        &[0x74, 0x10, 0x01, 0x00, 0x00, 0x00],
        &[0xfd, 0x70, 0x40, 0x00, 0x00, 0x00, 0x80],
    ];
    if len < NOPS.len() {
        return NOPS[len].to_vec();
    }
    let mut out = Vec::with_capacity(len);
    if len <= 0x7f {
        while out.len() + 2 <= len {
            out.extend_from_slice(&[0x2e, len as u8]);
        }
    } else {
        out.extend_from_slice(&[0x38, len as u8, (len >> 8) as u8]);
    }
    out.resize(len, 0x00);
    out
}

#[cfg(test)]
mod tests {
    use super::nop_fill;

    #[test]
    fn padding_has_the_requested_length() {
        for n in 0..300 {
            assert_eq!(nop_fill(n).len(), n);
        }
        assert_eq!(nop_fill(8), [0x2e, 8, 0x2e, 8, 0x2e, 8, 0x2e, 8]);
    }
}
