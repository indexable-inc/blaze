//! Texas Instruments MSP430 and MSP430X, the 16-bit low-power
//! microcontrollers, with the 20-bit MSP430X extensions. `EM_MSP430` (105).
//!
//! # Reference
//!
//! `msp430-elf-as` (binutils 2.47), which `tools/xas-diff/run.sh msp430
//! msp430x` compares against byte for byte, run with `-mcpu=430` or
//! `-mcpu=430x`. The instruction table is the reference's own
//! (`include/opcode/msp430.h`), and the encoder follows `msp430_operands` in
//! `gas/config/tc-msp430.c` case for case, since that function *is* the
//! definition of MSP430 GNU syntax: see [`insn`] for the instruction set,
//! [`operand`] for addressing, and [`reloc`] for which relocation each
//! operand gets.
//!
//! # Targets
//!
//! | Name | GNU as | ISA | `e_flags` |
//! |---|---|---|---|
//! | `msp430` | `-mcpu=430` | the original 27 instructions | 11 (`MSP430x11`) |
//! | `msp430x` | `-mcpu=430x` | adds the 20-bit MSP430X set | 45 (`MSP430X`) |
//! | `msp430xv2` | `-mcpu=430xv2`, and GNU as's default | as `msp430x`, refusing what the CPUXV2 cores cannot do | 45 |
//!
//! GNU as picks the ISA from `-mmcu` too, by looking the device up in a table;
//! the three names above are the three answers it can come to.
//!
//! # Objects
//!
//! GNU as sets `linkrelax` for this target: the GNU linker relaxes MSP430
//! code, so nothing in a code section may be resolved in advance. Every
//! reference from an executable section is relocated, even a jump to the
//! label before it, and names the label itself rather than its section;
//! a difference of two labels in a code section becomes an
//! `R_MSP430_SYM_DIFF` pair. rsasm writes the same.
//!
//! Every object also gets a `.MSP430.attributes` section recording the ISA
//! and the small code and data model, and, for a non-empty `.data` or `.bss`
//! (and some other names), an undefined reference to the C runtime's
//! `__crt0_*` routine that initialises it, which is how GNU as keeps unused
//! startup code out of a link.
//!
//! # Lexing
//!
//! `;` starts a comment anywhere, `#` only in the first column (it is the
//! immediate prefix), and `{` separates statements. Numbers may carry a
//! Renesas-style suffix (`0FFh`), which in GNU as also stops a leading zero
//! from meaning octal.
//!
//! # Deliberate differences from the reference
//!
//! Where the reference writes something no linker can make sense of, rsasm
//! refuses the source instead:
//!
//! * operands an instruction does not take, which the reference ignores
//!   (`nop r5`, `mov r5, r6, r7`);
//! * a `pushm` or `popm` count outside 1 to 16, which it folds into the
//!   opcode;
//! * a polymorphic branch to anything but a label, whose addend it drops
//!   (`beq lab+2` branches to `lab`).
//!
//! And where the object differs without the linked program differing:
//!
//! * The polymorphic branches (`jump`, `beq`, `bgt`, …) need GNU as's `-mP`,
//!   and are always taken in their long form, as GNU as takes them without
//!   `-mQ`; rsasm accepts them without an option.
//! * `R_MSP430_SYM_DIFF`'s addend is zero, which is what the reference writes
//!   for all but the last such pair in a section, where it writes the
//!   negated value of the subtrahend. The GNU linker reads neither.
//! * A number `.set` after the code that uses it is written into the field;
//!   the reference relocates the field against the symbol.
//! * The `__crt0_*` references a `.section` directive adds follow the
//!   undefined symbols the file relocates against in the symbol table, where
//!   the reference puts them where the directive is.
//! * A bare number is a register (`mov 5, r6` moves `r5`) by value here and by
//!   spelling in the reference, which reads `0b101` and `0x0` as numbers and
//!   `010` as octal.
//!
//! # Not implemented
//!
//! `-ml` (the large memory model, which only changes the attributes), the
//! interrupt-state `NOP` warnings and insertion (`-mn`, `-my`), the silicon
//! errata options, `-mQ` assembly-time relaxation, and the `.profiler`,
//! `.refsym` and `.cpu` directives.

pub(crate) mod encode;
pub(crate) mod insn;
pub(crate) mod operand;
pub(crate) mod reg;
pub(crate) mod reloc;

use crate::arch::{
    ArchState, Architecture, AsmCtx, CommentSyntax, Endian, InsnRequest, SameSectionRef, Syntax,
};
use crate::cursor::Cursor;
use crate::lexer::Punct;
use crate::section::{FixupKind, SectionFlags, Variant};

pub const NAMES: &[&str] = &["msp430", "msp430x", "msp430xv2"];

pub fn lookup(name: &str) -> Option<Box<dyn Architecture>> {
    let isa = match name {
        "msp430" => Isa::Msp430,
        "msp430x" => Isa::Msp430X,
        "msp430xv2" => Isa::Msp430Xv2,
        _ => return None,
    };
    Some(Box::new(Msp430 { isa }))
}

/// The instruction set, as GNU as's `-mcpu` selects it.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Isa {
    /// The original MSP430.
    Msp430,
    /// The MSP430X, with 20-bit addresses.
    Msp430X,
    /// The CPUXV2 cores, which differ from the MSP430X only in what the
    /// reference refuses: indirect addressing through the PC, rotating the
    /// PC, and `popm` into the status register.
    Msp430Xv2,
}

impl Isa {
    pub fn is_430x(self) -> bool {
        self != Isa::Msp430
    }
}

/// The repeat count a `rpt` left for the next instruction, kept in the low
/// byte of [`ArchState::private`]: positive for a count, negative for the
/// register that holds one, zero for none.
pub(crate) fn repeat_of(state: &ArchState) -> i8 {
    state.private as u8 as i8
}

/// [`ArchState::private`] with the pending repeat count set to `n`.
pub(crate) fn with_repeat(state: &ArchState, n: i8) -> u64 {
    (state.private & !0xff) | n as u8 as u64
}

/// A bit of [`Architecture::label_flags`]: the label is in an executable
/// section.
const LABEL_IN_CODE: u8 = 1;

/// The MSP430 backend. The three ISAs share one encoder, which checks what
/// each allows.
pub struct Msp430 {
    isa: Isa,
}

impl Architecture for Msp430 {
    fn name(&self) -> &'static str {
        match self.isa {
            Isa::Msp430 => "msp430",
            Isa::Msp430X => "msp430x",
            Isa::Msp430Xv2 => "msp430xv2",
        }
    }

    fn endian(&self) -> Endian {
        Endian::Little
    }

    fn pointer_bytes(&self, _state: &ArchState) -> u8 {
        // Pointers are 16 or 20 bits, but MSP430 objects are ELF32.
        4
    }

    fn initial_state(&self) -> ArchState {
        ArchState {
            bits: 16,
            syntax: Syntax::Att,
            features: 0,
            cpu_features: [0; 3],
            intel_register_prefix: false,
            used: 0,
            private: 0,
        }
    }

    fn supports_syntax(&self, syntax: Syntax) -> bool {
        syntax == Syntax::Att
    }

    fn elf_machine(&self) -> u16 {
        // `EM_MSP430`, which `msp430-elf-readelf -h` reports as "Texas
        // Instruments msp430 microcontroller".
        105
    }

    fn elf_osabi(&self) -> u8 {
        // `ELFOSABI_STANDALONE`, which `bfd/elf32-msp430.c` sets.
        255
    }

    fn elf_flags(&self, _state: &ArchState) -> u32 {
        // The BFD machine, which GNU as sets from the ISA alone:
        // `E_MSP430_MACH_MSP430x11` for the 430 and `E_MSP430_MACH_MSP430X`
        // for both 430X variants.
        if self.isa.is_430x() { 45 } else { 11 }
    }

    fn elf_attributes(&self, _state: &ArchState) -> Vec<crate::arch::AttrSection> {
        // What `msp430_md_finish` adds with `bfd_elf_add_proc_attr_int`:
        // `OFBA_MSPABI_Tag_ISA`, and the small code and data models.
        use crate::arch::AttrValue::Int;
        let isa = if self.isa.is_430x() { 2 } else { 1 };
        vec![crate::arch::AttrSection::attributes(
            ".MSP430.attributes",
            "mspabi",
            vec![(4, Int(isa)), (6, Int(1)), (8, Int(1))],
        )]
    }

    fn section_symbols(&self, name: &str) -> &'static [&'static str] {
        // `msp430_make_init_symbols`, which names the C runtime routine that
        // sets up each kind of section, so that it is only linked in when a
        // section needs it.
        let starts = |prefixes: &[&str]| prefixes.iter().any(|p| name.starts_with(p));
        if starts(&[".either.bss"]) {
            &["__crt0_init_bss", "__crt0_init_highbss"]
        } else if starts(&[".bss", ".lower.bss", ".gnu.linkonce.b."]) {
            &["__crt0_init_bss"]
        } else if starts(&[".either.data"]) {
            &["__crt0_movedata", "__crt0_move_highdata"]
        } else if starts(&[".data", ".lower.data", ".gnu.linkonce.d."]) {
            &["__crt0_movedata"]
        } else if starts(&[".upper.data"]) {
            &["__crt0_move_highdata"]
        } else if starts(&[".upper.bss"]) {
            &["__crt0_init_highbss"]
        } else if starts(&[".init_array"]) {
            &["__crt0_run_init_array", "__crt0_run_array"]
        } else if starts(&[".preinit_array"]) {
            &["__crt0_run_preinit_array", "__crt0_run_array"]
        } else if starts(&[".fini_array"]) {
            &["__crt0_run_fini_array", "__crt0_run_array"]
        } else {
            &[]
        }
    }

    fn common_symbols(&self) -> &'static [&'static str] {
        // `msp430_comm` and `msp430_lcomm`: common data lands in `.bss`.
        &["__crt0_init_bss"]
    }

    /// GNU as for MSP430 aligns nothing in `.bss` by its size.
    fn local_common(&self) -> crate::arch::LocalCommon {
        crate::arch::LocalCommon::PACKED
    }

    fn data_reloc(&self, size: u8, pcrel: bool) -> Option<u32> {
        reloc::data(self.isa, size, pcrel)
    }

    fn difference_relocs(&self, kind: &FixupKind) -> Option<(u32, u32)> {
        reloc::difference(self.isa, kind.size)
    }

    fn difference_subtrahend_first(&self) -> bool {
        // `tc_gen_reloc` returns the `SYM_DIFF` before the value.
        true
    }

    /// GNU as for MSP430 never folds a difference of two labels in a code
    /// section into a data field (`msp430_allow_local_subtract`), since the
    /// linker may relax the code between them. Numbered local labels are no
    /// exception: GNU as's test for its own labels (`S_IS_GAS_LOCAL`) looks
    /// for a name ending in `\001` or `\002`, and theirs end in a digit.
    fn defers_difference(&self, kind: &FixupKind, symbols_in: &SectionFlags, _moves: bool) -> bool {
        symbols_in.exec && Some(kind.reloc) == reloc::data(self.isa, kind.size, false)
    }

    /// A number as the target of a jump or a symbolic operand is an address,
    /// relocated against no symbol, as GNU as relocates every PC-relative
    /// field. Only a number defined after its use gets that far: one already
    /// known is encoded where it is read.
    fn pcrel_number_is_address(&self) -> bool {
        true
    }

    /// `msp430_insert_uleb128_fixes`: a `.uleb128` of a difference GNU as
    /// could not fold, which in one section means one of code labels.
    fn uleb128_difference_relocs(
        &self,
        symbols_in: &SectionFlags,
        _moves: bool,
    ) -> Option<(u32, u32)> {
        symbols_in.exec.then(|| reloc::uleb128(self.isa))
    }

    /// Every PC-relative fixup is left to the linker
    /// (`msp430_force_relocation_local`), in a data section too.
    fn defers_to_linker(&self, _r: &SameSectionRef<'_>) -> bool {
        true
    }

    fn label_flags(&self, _state: &mut ArchState, _name: &str, in_code: bool) -> u8 {
        if in_code { LABEL_IN_CODE } else { 0 }
    }

    /// A relocation against a label in a code section names the label, since
    /// relaxation may move it within its section (`msp430_fix_adjustable`).
    fn keeps_reloc_symbol(&self, flags: u8, _ty: crate::symbol::SymType) -> bool {
        flags & LABEL_IN_CODE != 0
    }

    fn comments(&self) -> CommentSyntax {
        CommentSyntax {
            anywhere: &[";"],
            line_start: &["#"],
        }
    }

    fn tune_lexer(&self, cfg: &mut crate::lexer::LexConfig) {
        // `line_separator_chars` is "{" in `gas/config/tc-msp430.c`, and the
        // port is built with `NUMBERS_WITH_SUFFIX`, which also turns off the
        // leading-zero octal reading: `#010` is ten.
        cfg.stmt_sep = vec!['{'];
        cfg.radix_suffix = true;
        cfg.octal_leading_zero = false;
    }

    fn word_bytes(&self) -> u8 {
        2
    }

    fn align_is_log2(&self) -> bool {
        true
    }

    fn pads_section_tail(&self, _flags: &SectionFlags) -> bool {
        true
    }

    /// GNU as's conventions, with an explicit address advance for every row
    /// since its linker relaxes code (`DWARF2_USE_FIXED_ADVANCE_PC`). It has
    /// no call frame information.
    fn dwarf(
        &self,
        _state: &ArchState,
        _format: crate::output::Format,
    ) -> crate::dwarf::DwarfTarget {
        crate::dwarf::DwarfTarget {
            fixed_advance_pc: true,
            ..crate::dwarf::DwarfTarget::lines_only(crate::dwarf::Flavor::Gnu, 1)
        }
    }

    fn nop_fill(&self, _state: &ArchState, len: u64) -> Vec<u8> {
        // GNU as pads code with zeros here, not with `nop` (`4303`).
        vec![0; len as usize]
    }

    fn is_mnemonic(&self, name: &str) -> bool {
        insn::is_mnemonic(name)
    }

    /// `.mspabi_attribute` and `.gnu_attribute`, which GCC writes into its
    /// output. GNU as only checks them against the options it was run with,
    /// and writes its own attributes whatever they say; so does rsasm.
    fn directive(&self, cx: &mut AsmCtx<'_>, name: &str, cur: &mut Cursor<'_>) -> bool {
        match name {
            ".mspabi_attribute" => attribute(cx, cur, self.isa, false),
            ".gnu_attribute" => attribute(cx, cur, self.isa, true),
            _ => return false,
        }
        true
    }

    fn assemble(&self, cx: &mut AsmCtx<'_>, req: &InsnRequest<'_>) -> Option<Vec<Variant>> {
        let mnemonic = cx.name(req.mnemonic).to_ascii_lowercase();
        insn::assemble(cx, req, &mnemonic, self.isa)
    }
}

/// Checks an attribute directive, `msp430_object_attribute` in the reference,
/// against the ISA and the small memory model rsasm assembles for.
fn attribute(cx: &mut AsmCtx<'_>, cur: &mut Cursor<'_>, isa: Isa, gnu: bool) {
    let span = cur.remaining_span();
    let number = |cx: &mut AsmCtx<'_>, cur: &mut Cursor<'_>| {
        let e = cx.expr_parser().parse(cur)?;
        cx.constant(e)
    };
    let tag = number(cx, cur);
    let value = if cur.eat_punct(Punct::Comma).is_some() {
        number(cx, cur)
    } else {
        None
    };
    let (Some(tag), Some(value)) = (tag, value) else {
        cx.error(span, "expected a tag and a value, both numbers");
        return;
    };
    let directive = if gnu {
        ".gnu_attribute"
    } else {
        ".mspabi_attribute"
    };
    if tag == 0 || value == 0 {
        cx.error(
            span,
            format!("`{directive}` needs a tag and a value that are not zero"),
        );
        return;
    }
    if gnu {
        // `Tag_GNU_MSP430_Data_Region` only means something in the large
        // model, and any other tag passes unchecked.
        return;
    }
    let msg = match (tag, value) {
        // `OFBA_MSPABI_Tag_ISA`
        (4, 1) if isa.is_430x() => {
            "the file was compiled for the 430 ISA, but this is an MSP430X target"
        }
        (4, 2) if !isa.is_430x() => {
            "the file was compiled for the 430X ISA, but this is an MSP430 target"
        }
        (4, 1 | 2) => return,
        (4, _) => "unknown value for the ISA attribute (tag 4)",
        // `OFBA_MSPABI_Tag_Code_Model` and `OFBA_MSPABI_Tag_Data_Model`
        (6 | 8, 1) => return,
        (6 | 8, 2) => {
            "the file was compiled for the large memory model, which rsasm does not assemble for"
        }
        (6 | 8, _) => "unknown value for a memory model attribute (tag 6 or 8)",
        _ => "unknown MSPABI attribute tag",
    };
    cx.error(span, msg);
}
