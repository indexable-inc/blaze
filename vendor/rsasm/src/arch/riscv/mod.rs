//! RISC-V, RV32 and RV64. `EM_RISCV`.
//!
//! The backend assembles RV32I/RV64I with the M, A, F, D and C extensions, in
//! the GNU as dialect, and expands the pseudo-instructions that most RISC-V
//! source is actually written in.
//!
//! Two things shape the code more than anything else.
//!
//! *Immediates are scattered.* Almost no field is contiguous, so the word is
//! built with the immediate zeroed and the placement lives in a scatter
//! function (see [`encode`]) that runs whether the value is known now or
//! arrives from the linker.
//!
//! *The C extension is a rewrite, not a set of new mnemonics.* Source says
//! `add a0, a0, a1`; the assembler emits two bytes because a compressed form
//! of exactly that instruction exists. [`compress`] does that as a peephole
//! over finished words, and branches — whose displacement is not known until
//! layout runs — instead offer both widths as relaxation candidates.

pub mod asm;
pub mod compress;
pub mod encode;
pub mod insn;
pub mod matint;
pub mod operand;
pub mod pseudo;
pub mod reg;
pub mod reloc;

use crate::arch::{ArchState, Architecture, AsmCtx, Endian, FlatModifier, InsnRequest, Syntax};
use crate::cursor::Cursor;
use crate::dwarf::{CfiTarget, DwarfTarget, Flavor, cfi, numbered_register};
use crate::lexer::TokKind;
use crate::section::Variant;
use asm::Asm;
use insn::Kind;
use operand::Operands;
use pseudo::Handled;

pub const NAMES: &[&str] = &["riscv32", "riscv64"];

pub fn lookup(name: &str) -> Option<Box<dyn Architecture>> {
    let xlen = match name {
        "riscv32" | "rv32" | "rv32i" | "rv32g" | "rv32gc" => 32,
        "riscv64" | "rv64" | "rv64i" | "rv64g" | "rv64gc" | "riscv" => 64,
        _ => return None,
    };
    Some(Box::new(Riscv { xlen }))
}

pub struct Riscv {
    xlen: u8,
}

/// `ArchState::features` bit 0 says whether the C extension may shorten what
/// is emitted, bit 1 whether `la` goes through the GOT, and bit 2 whether the
/// linker may relax what follows.
///
/// `.option push` / `.option pop` need a stack, and `ArchState` has no field
/// for one, so the word doubles as it: each push shifts everything left by a
/// level and copies the current settings into the bottom one, each pop shifts
/// right. A single set bit above the deepest level marks the bottom, which is
/// how an unmatched pop is noticed.
const RVC: u64 = 1;
const PIC: u64 = 2;
const RELAX: u64 = 4;
const LEVEL: u64 = RVC | PIC | RELAX;
const LEVEL_BITS: u32 = 3;
const STACK_BOTTOM: u64 = 1 << LEVEL_BITS;

/// How many `.option push` levels are open.
fn option_depth(features: u64) -> u32 {
    ((63 - features.leading_zeros()) / LEVEL_BITS).saturating_sub(1)
}

fn rvc_enabled(state: &ArchState) -> bool {
    state.features & RVC != 0
}

/// Whether `.option pic` is in effect.
pub(super) fn pic_enabled(state: &ArchState) -> bool {
    state.features & PIC != 0
}

/// Whether the linker may relax what is assembled here: `.option relax`,
/// which GNU as starts a file with.
fn relax_enabled(state: &ArchState) -> bool {
    state.features & RELAX != 0
}

impl Architecture for Riscv {
    fn name(&self) -> &'static str {
        if self.xlen == 64 {
            "riscv64"
        } else {
            "riscv32"
        }
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["rv32", "rv64", "riscv", "rv32gc", "rv64gc"]
    }

    fn endian(&self) -> Endian {
        Endian::Little
    }

    fn pointer_bytes(&self, _state: &ArchState) -> u8 {
        self.xlen / 8
    }

    fn initial_state(&self) -> ArchState {
        ArchState {
            bits: self.xlen,
            syntax: Syntax::Att,
            // `rv32gc` / `rv64gc` is what toolchains default to, so compressed
            // instructions are on unless `.option norvc` turns them off, and
            // linker relaxation is on as GNU as has it unless `.option
            // norelax` turns it off.
            features: STACK_BOTTOM | RVC | RELAX,
            cpu_features: [0; 3],
            intel_register_prefix: false,
            used: 0,
            private: 0,
        }
    }

    fn supports_syntax(&self, syntax: Syntax) -> bool {
        // There is only one RISC-V operand syntax; Intel mode is meaningless
        // here, so it is not offered.
        syntax == Syntax::Att
    }

    fn elf_machine(&self) -> u16 {
        243 // EM_RISCV
    }

    /// `.riscv.attributes`, whose `Tag_RISCV_arch` is the ISA string a
    /// linker checks one object against another by. Both references write
    /// the same one for this instruction set — `riscv64-elf-as
    /// -march=rv64imafdc`, and `llvm-mc -mattr=+m,+a,+f,+d,+c
    /// -riscv-add-build-attributes` — down to the extensions each implies
    /// (`zicsr`, `zmmul`, the `a` and `c` halves, and `zcf` on RV32, whose
    /// compressed set has the single-precision loads).
    fn elf_attributes(&self, _state: &ArchState) -> Vec<crate::arch::AttrSection> {
        let arch = if self.xlen == 32 {
            "rv32i2p1_m2p0_a2p1_f2p2_d2p2_c2p0_zicsr2p0_zmmul1p0_zaamo1p0_zalrsc1p0_zca1p0\
             _zcd1p0_zcf1p0"
        } else {
            "rv64i2p1_m2p0_a2p1_f2p2_d2p2_c2p0_zicsr2p0_zmmul1p0_zaamo1p0_zalrsc1p0_zca1p0\
             _zcd1p0"
        };
        vec![crate::arch::AttrSection::attributes(
            ".riscv.attributes",
            "riscv",
            vec![(TAG_ARCH, crate::arch::AttrValue::Str(arch.to_string()))],
        )]
    }

    /// `EF_RISCV_RVC | EF_RISCV_FLOAT_ABI_DOUBLE`: the `rv32gc`/`rv64gc`
    /// defaults with their `ilp32d`/`lp64d` ABIs. GNU ld refuses to link
    /// objects whose float ABIs differ, and the flag records the ISA the file
    /// started with, so `.option norvc` leaves it set, as in GNU as.
    fn elf_flags(&self, _state: &ArchState) -> u32 {
        0x5
    }

    fn align_is_log2(&self) -> bool {
        true
    }

    /// A relaxing linker deletes instructions, so an alignment it will move
    /// code under is left to it, as `riscv_frag_align_code` leaves it: the
    /// assembler reaches the width of the shortest instruction, two bytes
    /// with compressed instructions and four without, and the linker takes
    /// back as much of the padding after that as it needs to.
    ///
    /// Nothing is left to the linker where the alignment is no wider than
    /// that shortest instruction, since no relaxation can disturb it, nor
    /// under `.option norelax`, where there is no relaxation to disturb it.
    fn align_reloc(
        &self,
        state: &ArchState,
        align: u64,
    ) -> Option<(u64, crate::section::FixupKind)> {
        let unit = if rvc_enabled(state) { 2 } else { 4 };
        (relax_enabled(state) && align > unit).then(|| (unit, encode::kind_align()))
    }

    /// llvm-mc, the reference, aligns `.text` to the size of the shortest
    /// instruction in effect when the file starts: 2 bytes with compressed
    /// instructions, 4 without. GNU as does the same.
    fn section_align(
        &self,
        state: &ArchState,
        name: &str,
        _flags: &crate::section::SectionFlags,
    ) -> u64 {
        match name {
            ".text" if rvc_enabled(state) => 2,
            ".text" => 4,
            _ => 1,
        }
    }

    fn word_bytes(&self) -> u8 {
        4
    }

    fn data_reloc(&self, size: u8, pcrel: bool) -> Option<u32> {
        reloc::data(size, pcrel)
    }

    fn difference_relocs(&self, kind: &crate::section::FixupKind) -> Option<(u32, u32)> {
        reloc::difference(kind)
    }

    /// A relaxing linker deletes instructions, so a distance across one is
    /// not known here. Both references keep such a difference as its two
    /// symbols; what they disagree on is how hard they look. GNU as defers
    /// every difference of two labels in a code section
    /// (`TC_FORCE_RELOCATION_SUB_SAME` tests `SEC_CODE` and nothing else),
    /// whether or not anything between them can be shortened and even under
    /// `.option norelax`, while llvm-mc walks what lies between the two
    /// labels and folds the difference where none of it is an instruction a
    /// linker may rewrite. rsasm follows llvm-mc, the reference its RISC-V
    /// objects are compared against: a difference GNU as defers and this
    /// folds is one the linker would have worked out to the same number, so
    /// the linked program is the same either way.
    fn defers_difference(
        &self,
        kind: &crate::section::FixupKind,
        _symbols_in: &crate::section::SectionFlags,
        code_moves: bool,
    ) -> bool {
        code_moves && reloc::difference(kind).is_some()
    }

    /// The same for a `.uleb128`, which has no fixed width for a linker to
    /// add into and so is set outright. The value this file computed stays in
    /// the field, in as many bytes as it needs, which is what both references
    /// leave there.
    fn uleb128_difference_relocs(
        &self,
        _symbols_in: &crate::section::SectionFlags,
        code_moves: bool,
    ) -> Option<(u32, u32)> {
        code_moves.then_some((reloc::SUB_ULEB128, reloc::SET_ULEB128))
    }

    fn refuses_signed_leb128_difference(&self) -> bool {
        true
    }

    fn moves_code(&self, kind: &crate::section::FixupKind) -> bool {
        reloc::moves_code(kind)
    }

    fn cfa_advance_field(&self, size: u8, bits: u8) -> Option<crate::section::FixupKind> {
        encode::kind_cfa_advance(size, bits)
    }

    fn modifier_reloc(&self, name: &str, size: u8, pcrel: bool) -> Option<u32> {
        // `call foo@plt` is accepted for compatibility; it only renames the
        // relocation on the `auipc`/`jalr` pair.
        (name == "plt" && size == 8 && pcrel).then_some(reloc::CALL_PLT)
    }

    /// A static image has no PLT, so `call foo@plt` calls `foo`.
    fn flat_modifier(&self, name: &str) -> FlatModifier {
        if name == "plt" {
            FlatModifier::Plain
        } else {
            FlatModifier::LinkerOnly
        }
    }

    /// llvm-mc's conventions, as for every RISC-V encoding.
    ///
    /// Without linker relaxation, which rsasm does not do, llvm-mc knows
    /// every distance and writes plain address advances. With it (`-mattr=+relax`)
    /// it writes each one as a `R_RISCV_ADD`/`R_RISCV_SUB` pair for the linker
    /// to fix up, as GNU as does even with `-mno-relax`.
    fn dwarf(&self, _state: &ArchState, _format: crate::output::Format) -> DwarfTarget {
        let wide = self.xlen == 64;
        DwarfTarget {
            cfi: Some(CfiTarget {
                data_align: if wide { -8 } else { -4 },
                ra_column: 1,
                initial: vec![cfi::Insn::DefCfa(2, 0)],
                fde_encoding: 0x1b,
                eh_frame_align: if wide { 8 } else { 4 },
                cie_version: 1,
            }),
            ..DwarfTarget::lines_only(Flavor::Llvm, 1)
        }
    }

    /// The psABI DWARF numbering: integer registers 0-31 and floating-point
    /// registers 32-63, under their numbers or ABI names.
    fn dwarf_register(
        &self,
        _state: &ArchState,
        _format: crate::output::Format,
        name: &str,
    ) -> Option<u32> {
        const ABI: [&str; 32] = [
            "zero", "ra", "sp", "gp", "tp", "t0", "t1", "t2", "s0", "s1", "a0", "a1", "a2", "a3",
            "a4", "a5", "a6", "a7", "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s11",
            "t3", "t4", "t5", "t6",
        ];
        const FABI: [&str; 32] = [
            "ft0", "ft1", "ft2", "ft3", "ft4", "ft5", "ft6", "ft7", "fs0", "fs1", "fa0", "fa1",
            "fa2", "fa3", "fa4", "fa5", "fa6", "fa7", "fs2", "fs3", "fs4", "fs5", "fs6", "fs7",
            "fs8", "fs9", "fs10", "fs11", "ft8", "ft9", "ft10", "ft11",
        ];
        if name == "fp" {
            return Some(8);
        }
        if let Some(i) = ABI.iter().position(|r| *r == name) {
            return Some(i as u32);
        }
        if let Some(i) = FABI.iter().position(|r| *r == name) {
            return Some(32 + i as u32);
        }
        numbered_register(name, "x", 31)
            .or_else(|| numbered_register(name, "f", 31).map(|n| 32 + n))
    }

    fn nop_fill(&self, _state: &ArchState, len: u64) -> Vec<u8> {
        encode::nop_bytes(len as usize)
    }

    fn assemble(&self, cx: &mut AsmCtx<'_>, req: &InsnRequest<'_>) -> Option<Vec<Variant>> {
        let mnemonic = cx.name(req.mnemonic).to_ascii_lowercase();
        let rvc = rvc_enabled(cx.state);
        let cur = req.cursor();
        let ops = Operands::parse(&cur, req.span);
        let relax = relax_enabled(cx.state);
        let mut a = Asm::new(cx, self.xlen, rvc, relax, req.span);

        match pseudo::expand(&mut a, &mnemonic, &ops) {
            Handled::Done => return Some(a.finish()),
            Handled::Failed => return None,
            Handled::No => {}
        }

        let (base, ordering) = split_ordering(&mnemonic);
        // A three-operand mnemonic whose last operand is not a register is
        // the immediate form: `and a0, a1, 4` is `andi`, `csrrs a0, frm, 3`
        // is `csrrsi` (see `insn::immediate_alias`).
        let base = match insn::immediate_alias(base) {
            Some(alias) if ops.len() == 3 && !ops.is_reg(a.cx, 2) => alias,
            _ => base,
        };
        let Some(def) = insn::lookup(base) else {
            a.error(
                req.mnemonic_span,
                format!("unknown instruction `{mnemonic}`"),
            );
            return None;
        };
        a.encode_def(def, base, &ops, ordering)?;
        Some(a.finish())
    }

    fn directive(&self, cx: &mut AsmCtx<'_>, name: &str, cur: &mut Cursor<'_>) -> bool {
        if name == ".attribute" {
            attribute(cx, cur);
            return true;
        }
        if name != ".option" {
            return false;
        }
        let tok = cur.peek();
        cur.set_pos(cur.all().len());
        let TokKind::Ident(n) = tok.kind else {
            cx.error(tok.span, "`.option` needs a name");
            return true;
        };
        let word = cx.name(n).to_ascii_lowercase();
        let features = cx.state.features;
        match word.as_str() {
            "rvc" => cx.state.features |= RVC,
            "norvc" => cx.state.features &= !RVC,
            "pic" => cx.state.features |= PIC,
            "nopic" => cx.state.features &= !PIC,
            "relax" => cx.state.features |= RELAX,
            "norelax" => cx.state.features &= !RELAX,
            "push" if option_depth(features) >= 20 => {
                cx.error(tok.span, "`.option push` nested more than 20 deep");
            }
            "push" => cx.state.features = (features << LEVEL_BITS) | (features & LEVEL),
            "pop" if option_depth(features) == 0 => {
                cx.error(tok.span, "`.option pop` with no `.option push`");
            }
            "pop" => cx.state.features = features >> LEVEL_BITS,
            // `.option arch` carries an extension list this backend does not
            // track.
            "arch" => {}
            _ => cx.error(tok.span, format!("unknown `.option {word}`")),
        }
        true
    }
}

/// `Tag_RISCV_arch`, the ISA string.
const TAG_ARCH: u32 = 5;

/// The names `.attribute` takes for a tag, and the tag each names, from GNU
/// as's own table (`riscv_convert_symbolic_attribute` in `tc-riscv.c`) with
/// the numbers `include/elf/riscv.h` gives them. Each is also spelled with a
/// `Tag_RISCV_` prefix there, which is stripped before the lookup.
const TAG_NAMES: &[(&str, u32)] = &[
    ("stack_align", 4),
    ("arch", TAG_ARCH),
    ("unaligned_access", 6),
    ("priv_spec", 8),
    ("priv_spec_minor", 10),
    ("priv_spec_revision", 12),
];

/// `.attribute <tag>, <value>`: the tag is a number or one of the names
/// above, and the value a number or, for `arch`, a string.
///
/// The ISA string is written as the source gave it. GNU as reads it and
/// writes back what it makes of it, which for a string that is already
/// canonical — every extension with a version, and every implied extension
/// spelled out, as both references write one — is the same thing; a string
/// that leaves something out is not expanded here.
fn attribute(cx: &mut AsmCtx<'_>, cur: &mut Cursor<'_>) {
    let tok = cur.peek();
    let tag = match tok.kind {
        TokKind::Int(n) if n <= u64::from(u32::MAX) => {
            cur.advance();
            n as u32
        }
        TokKind::Ident(n) => {
            cur.advance();
            let word = cx.name(n).to_ascii_lowercase();
            let word = word.strip_prefix("tag_riscv_").unwrap_or(&word);
            match TAG_NAMES.iter().find(|&&(name, _)| name == word) {
                Some(&(_, tag)) => tag,
                None => {
                    cx.error(tok.span, format!("`.attribute` does not know `{word}`"));
                    cur.set_pos(cur.all().len());
                    return;
                }
            }
        }
        _ => {
            cx.error(tok.span, "`.attribute` expects a tag");
            cur.set_pos(cur.all().len());
            return;
        }
    };
    if !cur.peek().is_punct(crate::lexer::Punct::Comma) {
        let span = cur.peek().span;
        cx.error(span, "`.attribute` expects a comma and a value");
        cur.set_pos(cur.all().len());
        return;
    }
    cur.advance();
    let span = cur.peek().span;
    // Only the ISA string is a string; the rest are numbers, and GNU as
    // refuses the other spelling for either.
    let wants_string = tag == TAG_ARCH;
    let value = match cur.peek().kind {
        TokKind::Int(n) if !wants_string => {
            cur.advance();
            crate::arch::AttrValue::Int(n)
        }
        TokKind::Str(i) if wants_string => {
            cur.advance();
            crate::arch::AttrValue::Str(String::from_utf8_lossy(cx.pool.get(i)).into_owned())
        }
        _ if wants_string => {
            cx.error(span, format!("`.attribute` tag {tag} takes a string"));
            cur.set_pos(cur.all().len());
            return;
        }
        _ => {
            cx.error(span, format!("`.attribute` tag {tag} takes a number"));
            cur.set_pos(cur.all().len());
            return;
        }
    };
    cx.requests.push(crate::arch::Request::Attribute {
        vendor: "riscv",
        tag,
        value,
    });
}

/// Splits the `.aq` / `.rl` ordering suffix off an atomic mnemonic.
///
/// The suffix is part of the mnemonic rather than an operand, and it sets two
/// bits that sit immediately below the `funct5` already in the base word.
fn split_ordering(mnemonic: &str) -> (&str, u32) {
    for (suffix, bits) in [(".aqrl", 3u32), (".aq", 2), (".rl", 1)] {
        if let Some(base) = mnemonic.strip_suffix(suffix)
            && insn::lookup(base).is_some_and(|d| matches!(d.kind, Kind::Amo | Kind::AmoLoad))
        {
            return (base, bits << 25);
        }
    }
    (mnemonic, 0)
}
