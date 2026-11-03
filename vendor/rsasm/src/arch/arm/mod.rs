//! ARM 32-bit: the A32 (ARM) and T32 (Thumb) instruction sets. `EM_ARM`.
//!
//! The two instruction sets share a register file, a condition-code table and
//! an operand grammar, but not an encoding, so they share everything up to
//! [`Insn`] and then split into [`encode`] (A32) and [`thumb`] (T32). Which
//! one runs is a property of the *state*, not of the backend object: `.arm`
//! and `.thumb` switch between them mid-file, exactly as `.code32` and
//! `.code16` do on x86, and they are spelled that way too.
//!
//! The state's `bits` field carries the choice — 32 for ARM, 16 for Thumb —
//! because that is what `.code 16` and `.code 32` already mean in ARM sources.
//!
//! # Position-independent operands
//!
//! ARM writes the relocation a reference needs as a suffix in parentheses,
//! `sym(GOT)`, where the rest of the GNU syntax writes `sym@GOT`, and builds
//! a 32-bit address out of a `movw`/`movt` pair whose operands are written
//! `#:lower16:sym` and `#:upper16:sym`. GNU as reads the suffix in exactly
//! two places, and this backend reads it in the same two: in `.word` and
//! `.long`, which its `s_arm_elf_cons` handles, and on the target of a `b`,
//! `bl` or `blx`, whose operand type `parse_operands` gives a `parse_reloc`
//! call. A `(plt)` on a branch changes nothing about the object — GNU as
//! asks for `R_ARM_PLT32` and then writes the `R_ARM_CALL` or
//! `R_ARM_JUMP24` a plain branch would get, since a linker routes either
//! through a PLT entry when it needs one — so the suffix is accepted and the
//! branch is relocated as it always was.
//!
//! A `_GLOBAL_OFFSET_TABLE_` in a four-byte data field is `R_ARM_BASE_PREL`,
//! the distance from the field to the GOT, whether it was written as a
//! difference against a label or on its own; that is `md_apply_fix`'s rule,
//! and it is what makes `.word _GLOBAL_OFFSET_TABLE_ - (1b + 8)` next to an
//! `add rn, pc, rn` load the GOT's address.
//!
//! Three suffixes name a word whose meaning the platform ABI rather than the
//! assembler settles. `(SBREL)` is the offset from the static base a register
//! holds in code built with `-msingle-pic-base`. `(TARGET1)` and `(TARGET2)`
//! are the two words the ABI leaves a platform to define — on GNU systems the
//! first links as `R_ARM_ABS32` and the second as `R_ARM_GOT_PREL` — which is
//! what lets one C++ runtime source serve platforms that want an absolute
//! `.init_array` entry and ones that want a relative one. All three are
//! `.word` and `.long` suffixes only, as the GOT ones are.
//!
//! # Thread-local operands
//!
//! The access models are data suffixes like the GOT ones: `(TLSGD)`,
//! `(TLSLDM)`, `(TLSLDO)`, `(GOTTPOFF)`, `(TPOFF)` and `(TLSDESC)`, each an
//! `R_ARM_TLS_*` relocation that names the variable rather than its section
//! and makes an undefined one `STT_TLS`. What a compiler writes adds the
//! distance from the `add rn, pc, rn` that uses the word,
//! `.word x(TLSGD) + (. - .LPIC0 - 8)`; that distance is a number once the
//! layout is known, and GNU as relocates `x` with it as the addend, not
//! PC-relative. The fields are `REL`, and GNU as fills two of them other than
//! with the addend alone; see `Arm::rel_field`.
//!
//! `(tlscall)` on a branch target is the call to a TLS descriptor's resolver,
//! `R_ARM_TLS_CALL` or `R_ARM_THM_TLS_CALL`, and leaves the branch's
//! displacement zero. `.tlsdescseq sym` marks the instruction after it, which
//! a linker may rewrite, with `R_ARM_TLS_DESCSEQ` or `R_ARM_THM_TLS_DESCSEQ`,
//! a relocation that covers none of its bits. GNU as also takes both as data
//! suffixes, `(TLSCALL)` and `(TLSDESCSEQ)`, and so does this backend.
//!
//! # Deliberate differences from GNU as
//!
//! GNU as is the reference for what this backend writes — its literal pools,
//! its mapping symbols and its interworking — but not for what it accepts.
//! Two things it refuses are assembled here, and llvm-mc, which writes the
//! same object rsasm does for both, is the reference for them instead.
//! Two more are places GNU as writes bytes that are not the program the
//! source states.
//!
//! * A branch to a *local* label in another section that is an odd number of
//!   halfwords into Thumb code. GNU as's `arm_fix_adjustable` relocates such
//!   a reference against the label's section, folding the label's offset into
//!   the addend, and `md_apply_fix` then reads the low two bits of that
//!   addend as the branch destination's and stops with "misaligned branch
//!   destination" — although the offset it checked is not the branch's until
//!   a linker has placed both sections. rsasm names the label, as llvm-mc
//!   does (see `relocates_with_label` below), and leaves the whole reference
//!   to the linker, which has both addresses and the `blx` to reach Thumb
//!   with. The case is in `tools/mc-diff/arm-relocs.txt`.
//! * A literal pool entry holding a difference of labels, `ldr r0, =l1-l0`,
//!   where the difference is not already a number. GNU as's
//!   `parse_big_immediate` takes a constant, a bignum, or a symbol plus an
//!   addend, and a difference of two labels that the parser cannot fold is
//!   none of the three, so the line is a syntax error there; once the layout
//!   is known it is an ordinary number, and rsasm puts it in the pool. The
//!   case is in `tools/mc-diff/arm-programs.txt`.
//! * A `.arm` or a `.thumb` in the middle of an `it` block that
//!   `-mimplicit-it` made up. GNU as keeps `now_pred` per section rather than
//!   per instruction set, so the block spans the switch and the A32
//!   instructions after it are counted into it. Two things follow, both of
//!   them bytes no architecture can mean: the `it`'s mask grows to cover an
//!   A32 instruction, which leaves the Thumb instruction that was meant to
//!   be the block's second one outside it and assembled unconditionally
//!   (`addeq r0, r1, r2` becomes an `adds`); and an A32 instruction whose
//!   condition the open block cannot hold makes
//!   `new_automatic_it_block` write a Thumb `it` halfword, and a `$t`
//!   mapping symbol, into the middle of ARM code, so that every A32
//!   instruction after it is two bytes off a word boundary. A made-up block
//!   ends at the switch here, which leaves each instruction set predicated
//!   the way it predicates itself. Neither assembler is a reference for the
//!   other, so the case is in `tests/arm_gas.rs` rather than in a corpus,
//!   and `tools/fuzz/arm-programs.py` does not write one.
//! * The offset of a T32 coprocessor transfer on coprocessor 9 with P set
//!   and W clear — `ldc p9, c1, [r0, #8]`, and every `ldc`, `stc` and label
//!   form of that shape. `md_apply_fix` tells the half-precision
//!   `vldr`/`vstr`, whose offset counts halfwords, from the word-counting
//!   rest by the finished word, `(newval & 0x0f200f00) == 0x0d000900`, which
//!   an `ldc` of coprocessor 9 matches as well; GNU as then writes half the
//!   field the architecture asks for, and accepts an offset that is only a
//!   multiple of two. The bytes it writes read back as a different address,
//!   so rsasm counts words, as llvm-mc does. The case is in
//!   `tools/mc-diff/thumb-programs.txt`.
//!
//! The relocation suffixes differ in what each assembler will read, rather
//! than in what either writes. `s_arm_elf_cons` strikes the suffix out of the
//! line and parses the rest again, so `.word sym(GOT) + 4` is `.word sym + 4`
//! relocated as a GOT entry; here the suffix binds to the symbol it follows,
//! which comes to the same relocation and addend for every expression either
//! assembler resolves, and differs only at the edges. `.word sym(PLT) + 4` is
//! a syntax error in GNU as, whose `(plt)` case emits the symbol and stops
//! reading, and is `.word sym + 4` here. `.4byte` and `.int` are plain
//! four-byte directives in GNU as, which gives the suffix only to `.word` and
//! `.long`, and take it here. A suffix GNU as knows but this backend has no
//! relocation for — the FDPIC ones, which GNU as itself refuses outside an
//! FDPIC object — is refused as unrecognised. And `sym(GOT) - label` is
//! refused, as is the difference with any other suffix and
//! `.tlsdescseq sym - label`: none of their relocations
//! has a PC-relative counterpart for the difference to become, and GNU as,
//! which makes the fixup PC-relative regardless, writes the field's own
//! address into the field.

#[doc(hidden)]
pub mod attr_data;
pub(crate) mod attrs;
pub(crate) mod cpu;
#[doc(hidden)]
pub mod cpu_data;
pub mod encode;
pub(crate) mod generic;
pub mod imm;
pub mod insn;
pub mod operand;
pub mod reg;
pub mod reloc;
#[doc(hidden)]
pub mod table;
pub mod thumb;
pub(crate) mod vfp;

use crate::arch::{
    ArchState, Architecture, AsmCtx, CpuOption, Endian, InsnRequest, Interwork, InterworkTarget,
    Request, Syntax,
};
use crate::cursor::Cursor;
use crate::dwarf::{CfiTarget, DwarfTarget, Flavor, cfi, numbered_register};
use crate::lexer::{Punct, TokKind};
use crate::section::{FixupKind, LinkValue, Variant};
use crate::source::Span;
use crate::symbol::SymType;
use insn::{AL, Mnem, Width};
use operand::Operand;

pub const NAMES: &[&str] = &["arm", "thumb"];

pub fn lookup(name: &str) -> Option<Box<dyn Architecture>> {
    let thumb = match name {
        "arm" | "armv7" | "armv7-a" | "arm32" => false,
        "thumb" | "thumbv7" | "thumb2" => true,
        _ => return None,
    };
    Some(Box::new(Arm { thumb }))
}

/// One instruction, after the mnemonic has been taken apart and the operands
/// parsed. Both encoders work from this.
#[derive(Copy, Clone)]
pub struct Insn<'o> {
    /// In a Thumb `it` block, whose condition the instruction takes: the
    /// 16-bit data-processing forms then leave the flags alone.
    pub in_it: bool,
    pub mnem: Mnem,
    pub cond: u8,
    /// Whether the source wrote a condition, as opposed to defaulting to `al`.
    /// Some encodings exist only unconditionally and need to tell the two
    /// apart.
    pub cond_written: bool,
    pub set_flags: bool,
    pub width: Width,
    /// The mnemonic as written, for diagnostics.
    pub text: &'o str,
    pub ops: &'o [Operand],
    pub span: Span,
}

pub struct Arm {
    /// Whether a fresh state starts in Thumb mode.
    thumb: bool,
}

/// The state's `bits` value that means Thumb.
const THUMB_BITS: u8 = 16;

/// The `sym(NAME)` suffixes a `.word` takes; see
/// [`Arm::data_paren_modifiers`].
const DATA_SUFFIXES: &[&str] = &[
    "got",
    "got_prel",
    "gotoff",
    "sbrel",
    "target1",
    "target2",
    "plt",
    "tlsgd",
    "tlsldm",
    "tlsldo",
    "gottpoff",
    "tpoff",
    "tlsdesc",
    "tlscall",
    "tlsdescseq",
];

/// The ones a branch target takes, which are the two GNU as's
/// `encode_branch` allows.
const BRANCH_SUFFIXES: &[&str] = &["plt", "tlscall"];

/// Label flag: defined in Thumb code.
const LABEL_THUMB: u8 = 1;
/// Label flag: named by `.thumb_func`.
const LABEL_THUMB_FUNC: u8 = 2;

/// `ArchState::private` bit: a `.thumb_func` is waiting for its label.
const PENDING_THUMB_FUNC: u64 = 1;

/// Whether a label is a Thumb function, the way GNU as's `THUMB_IS_FUNC`
/// decides it for EABI objects: named by `.thumb_func`, or a function by
/// `.type` and defined in Thumb code.
pub fn thumb_is_func(flags: u8, ty: SymType) -> bool {
    flags & LABEL_THUMB_FUNC != 0 || (flags & LABEL_THUMB != 0 && ty == SymType::Func)
}

/// Whether a label is an ARM function: a function by `.type`, defined in ARM
/// code.
pub fn arm_is_func(flags: u8, ty: SymType) -> bool {
    flags & LABEL_THUMB == 0 && ty == SymType::Func
}

// The interwork classes of branch fixups; see `Arm::interwork`.
/// ARM `bl label`.
pub const IW_ARM_BL: u8 = 1;
/// ARM `blx label`.
pub const IW_ARM_BLX: u8 = 2;
/// ARM `b`, `b<cond>` and `bl<cond>`.
pub const IW_ARM_JUMP: u8 = 3;
/// Thumb `bl label`.
pub const IW_THUMB_BL: u8 = 4;
/// Thumb `blx label`.
pub const IW_THUMB_BLX: u8 = 5;
/// Thumb 32-bit `b.w` and `b<cond>.w`.
pub const IW_THUMB_JUMP: u8 = 6;
/// Thumb 16-bit `b` and `b<cond>`, which have no relocation.
pub const IW_THUMB_JUMP16: u8 = 7;
/// The 16-bit form of a Thumb `adr` layout may widen.
pub const IW_THUMB_ADR16: u8 = 8;
/// The 32-bit form of a Thumb `adr` layout chose between the two, whose
/// addend is even, so setting the Thumb bit in it adds one.
pub const IW_THUMB_ADR: u8 = 9;
/// [`IW_THUMB_ADR`] where the addend is already odd, so setting the Thumb bit
/// in it changes nothing.
pub const IW_THUMB_ADR_ODD: u8 = 10;
/// The 16-bit form of a Thumb `ldr rt, label` layout may widen. It shares
/// `relax_adr` with the `adr` above, so a Thumb function widens it too,
/// although the load leaves the address alone.
pub const IW_THUMB_LDR16: u8 = 11;
/// A PC-relative reference GNU as resolves within the section and has no
/// relocation for: a load, a store, a preload or a coprocessor transfer that
/// names a label, an A32 `adr` or `adrl`, and a Thumb `adr.n`. A global
/// target in the same section it resolves, so only the weak one it refuses
/// is of interest.
pub const IW_STRONG_ONLY: u8 = 12;
/// A 32-bit Thumb `adr` layout will not resize, which is the one such
/// reference GNU as writes for a weak target rather than refusing; see
/// [`crate::section::FixupKind::without_symbol`].
pub const IW_THUMB_ADR32: u8 = 13;

impl Architecture for Arm {
    fn name(&self) -> &'static str {
        if self.thumb { "thumb" } else { "arm" }
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["armv7", "armv7-a", "arm32", "thumbv7", "thumb2"]
    }

    fn endian(&self) -> Endian {
        Endian::Little
    }

    fn pointer_bytes(&self, _state: &ArchState) -> u8 {
        4
    }

    fn initial_state(&self) -> ArchState {
        ArchState {
            bits: if self.thumb { THUMB_BITS } else { 32 },
            syntax: Syntax::Att,
            features: 0,
            cpu_features: cpu::initial(),
            intel_register_prefix: false,
            used: 0,
            private: 0,
        }
    }

    fn supports_syntax(&self, syntax: Syntax) -> bool {
        // ARM has one operand syntax; the x86 Intel/AT&T split does not apply.
        syntax == Syntax::Att
    }

    fn elf_machine(&self) -> u16 {
        40
    }

    /// `.ARM.attributes`, which GNU as adds to every object and both
    /// `objdump` and GNU ld read to decide what the program may contain.
    /// Without it a disassembler assumes a default CPU, so an `e12fff1e`
    /// prints as `msr SP_hyp, lr, lsl pc` rather than `bx lr`, and the
    /// linker assumes the oldest architecture: it routes every ARM/Thumb
    /// call through an interworking veneer instead of turning it into `blx`,
    /// and replaces a branch to an undefined weak symbol with `mov r0, r0`
    /// rather than the ARMv6T2 `nop`. Both showed up as different linked
    /// bytes in `tools/link-diff`.
    ///
    /// What goes in it is the CPU and floating-point unit selected, which
    /// starts as this backend's own — `-march=armv7ve -mfpu=neon-vfpv4`,
    /// what `tools/xas-diff` assembles the reference with — and follows
    /// `.arch`, `.cpu`, `.fpu`, `.arch_extension`, `.object_arch` and
    /// `.eabi_attribute` from there. See [`attrs`].
    fn elf_attributes(&self, state: &ArchState) -> Vec<crate::arch::AttrSection> {
        attrs::section(state)
    }

    /// `.arch` and `.cpu` name one of GNU as's ARM CPUs here rather than
    /// another backend. Two tables read the name — the build attributes' and
    /// the feature model's — and each is derived from binutils its own way,
    /// so a name either of them has is one this backend claims.
    fn selects_cpu(&self, state: &mut ArchState, name: &str, cpu: bool) -> Result<bool, String> {
        let attr = if cpu {
            attrs::set_cpu(state, name)
        } else {
            attrs::set_arch(state, name)
        };
        Ok(cpu::directive_arch(state, name, cpu) || attr)
    }

    /// `-march=`, `-mcpu=` and `-mfpu=`, which take the `+name` extension
    /// suffixes the directives do not; see [`cpu`].
    fn select_option(
        &self,
        state: &mut ArchState,
        opt: CpuOption,
        name: &str,
    ) -> Result<(), String> {
        cpu::option(state, opt, name)?;
        // A target with no ARM instruction set at all starts in Thumb, which
        // is what GNU as's `autoselect_thumb_from_cpu_variant` does with an
        // M-profile `-mcpu` or `-march`.
        if !cpu::has(state, cpu_data::V1) {
            state.bits = THUMB_BITS;
        }
        // The attributes follow the selection the way the directives leave
        // them, which for `-mfpu=` is not quite what GNU as writes: the
        // option merges the unit into the CPU's own where the directive
        // replaces it, and `attrs` is measured through the directive. A name
        // `attr_data` has not is one GNU as wrote no attributes for, and the
        // feature model above has already taken it.
        let (base, exts) = match name.split_once('+') {
            Some((base, rest)) => (base, rest),
            None => (name, ""),
        };
        let _ = match opt {
            CpuOption::Cpu => attrs::set_cpu(state, base),
            CpuOption::Fpu => attrs::set_fpu(state, base),
            _ => attrs::set_arch(state, base),
        };
        for ext in exts.split('+').filter(|e| !e.is_empty()) {
            attrs::set_extension(state, ext);
        }
        Ok(())
    }

    /// `EF_ARM_EABI_VER5`, as llvm-mc writes for `arm-linux-gnueabi`; GNU ld
    /// refuses to mix EABI versions, and version 0 is not an EABI object.
    fn elf_flags(&self, _state: &ArchState) -> u32 {
        0x0500_0000
    }

    fn align_is_log2(&self) -> bool {
        true
    }

    /// GNU as, the reference for ARM objects, gives a section no alignment
    /// of its own: the first instruction assembled into it raises it, to 4
    /// bytes for ARM and 2 for Thumb (see `code_mapping`), where llvm-mc
    /// aligns `.text` to 4 bytes from the start. `tools/mc-diff` records
    /// the difference.
    fn section_align(
        &self,
        _state: &ArchState,
        _name: &str,
        _flags: &crate::section::SectionFlags,
    ) -> u64 {
        1
    }

    /// GNU as relocates a branch to a global or weak symbol in the same
    /// section, but resolves a field that has no relocation even then: a
    /// literal load, an `adr`, and a 16-bit branch to a symbol no other
    /// object can replace (`interwork` widens the others).
    fn defers_to_linker(&self, r: &crate::arch::SameSectionRef<'_>) -> bool {
        r.reloc != 0 && r.binding != crate::symbol::Binding::Local
    }

    fn word_bytes(&self) -> u8 {
        4
    }

    /// ARM writes immediates as `#1`, so `#` is a comment only in the first column and `@` takes its place elsewhere.
    fn comments(&self) -> crate::arch::CommentSyntax {
        crate::arch::CommentSyntax {
            anywhere: &["@", "//"],
            line_start: &["#"],
        }
    }

    fn data_reloc(&self, size: u8, pcrel: bool) -> Option<u32> {
        reloc::data(size, pcrel)
    }

    /// The suffixes `s_arm_elf_cons` reads after a symbol in a `.word`, of
    /// which these are the ones this backend has a relocation for; see
    /// [`reloc::modifier`]. GNU as knows more — `(TARGET1)`, `(TARGET2)`,
    /// `(SBREL)` and the FDPIC ones — and refuses anything not in its table,
    /// which is what an unlisted name gets here.
    fn data_paren_modifiers(&self) -> &'static [&'static str] {
        DATA_SUFFIXES
    }

    fn modifier_reloc(&self, name: &str, size: u8, pcrel: bool) -> Option<u32> {
        reloc::modifier(name, size, pcrel)
    }

    /// `(PLT)` on a branch target is the relocation the branch already has:
    /// GNU as asks for `R_ARM_PLT32` and then writes `R_ARM_CALL` or
    /// `R_ARM_JUMP24`, the same numbers a plain `bl sym` and `b sym` get,
    /// because a linker routes either through a PLT entry when it needs one.
    /// Returning the fixup's own relocation says so.
    ///
    /// `(tlscall)` is `R_ARM_TLS_CALL` on every ARM branch `encode_branch`
    /// encodes, conditional or not, and `R_ARM_THM_TLS_CALL` on a Thumb `bl`
    /// or `blx`. A Thumb `b` goes through `do_t_branch` instead, which reads
    /// the operand the same way and then ignores the suffix, so it keeps its
    /// own relocation. The zero-width fixup `.tlsdescseq` makes already
    /// carries the relocation for its instruction set.
    fn fixup_modifier_reloc(
        &self,
        name: &str,
        kind: &crate::section::FixupKind,
    ) -> crate::arch::FixupModifier {
        use crate::arch::FixupModifier;
        match name {
            "plt" if kind.pcrel => return FixupModifier::Reloc(kind.reloc),
            "tlscall" if kind.pcrel => {
                return FixupModifier::Reloc(match kind.reloc {
                    reloc::CALL | reloc::JUMP24 => reloc::TLS_CALL,
                    reloc::THM_CALL => reloc::THM_TLS_CALL,
                    other => other,
                });
            }
            "tlsdescseq" if kind.size == 0 => return FixupModifier::Reloc(kind.reloc),
            _ => {}
        }
        match reloc::modifier(name, kind.size, kind.pcrel) {
            Some(r) => FixupModifier::Reloc(r),
            None => FixupModifier::Unknown,
        }
    }

    /// Every thread-local suffix makes its symbol `STT_TLS`, which GNU as's
    /// `md_apply_fix` does for each of their relocations; none of them adds
    /// `_GLOBAL_OFFSET_TABLE_` to the object.
    fn modifier_symbols(&self, name: &str) -> crate::arch::ModifierSymbols {
        crate::arch::ModifierSymbols {
            tls: reloc::is_tls_modifier(name),
            ..Default::default()
        }
    }

    /// `.tlsdescseq`'s relocation covers no bytes, so there is no field to
    /// hold anything.
    fn addend_in_field(&self, reloc: u32, rela: bool) -> bool {
        !rela && !matches!(reloc, reloc::TLS_DESCSEQ | reloc::THM_TLS_DESCSEQ)
    }

    /// The call and descriptor-sequence marks leave no addend at all; see
    /// [`reloc::has_no_addend`]. Writing zero, rather than leaving the field
    /// alone, gives a Thumb `bl` the `J1` and `J2` bits a displacement of
    /// zero has.
    ///
    /// GNU as also leaves two thread-local offsets in their fields other than
    /// as the addend alone, and rsasm writes what it does, since the object is
    /// what `tools/xas-diff` compares.
    ///
    /// `tc_gen_reloc` takes the symbol's value back out of the addend BFD
    /// computed for most of them, but not for `R_ARM_TLS_LDO32`, whose howto
    /// then puts it back into the field: `x(TLSLDO) + 4`, for an `x` four
    /// bytes into `.tdata`, leaves 8 there, and GNU ld, which adds the symbol
    /// again, links it as 12 bytes into the block where llvm-mc's object
    /// links as 8. An addend of zero writes nothing, so the `x(TLSLDO)` a
    /// compiler writes is unaffected.
    ///
    /// `R_ARM_TLS_LDM32` does have the value taken out, and is written
    /// through the same howto, which writes nothing where what is left is
    /// zero: an addend equal to the symbol's value leaves the field zero.
    fn rel_field(&self, reloc: u32, addend: i64, symbol_value: i64, _by_section: bool) -> i64 {
        match reloc {
            reloc::TLS_LDO32 if addend != 0 => addend.wrapping_add(symbol_value),
            reloc::TLS_LDM32 if addend == symbol_value => 0,
            r if reloc::has_no_addend(r) => 0,
            _ => addend,
        }
    }

    /// `(PLT)` names the function itself where nothing built a PLT; the GOT
    /// suffixes name a slot in a table only a linker writes.
    fn flat_modifier(&self, name: &str) -> crate::arch::FlatModifier {
        if name == "plt" {
            crate::arch::FlatModifier::Plain
        } else {
            crate::arch::FlatModifier::LinkerOnly
        }
    }

    /// `:lower16:(sym - label)` is the PC-relative half, which is a
    /// relocation of its own rather than the absolute one with a difference
    /// in it.
    fn pcrel_reloc(&self, reloc: u32, size: u8) -> Option<u32> {
        if let Some(half) = reloc::half_pcrel(reloc) {
            return Some(half);
        }
        if reloc != 0 && Some(reloc) == reloc::data(size, false) {
            return reloc::data(size, true);
        }
        None
    }

    /// GNU as's `md_apply_fix` turns a four-byte data reference to
    /// `_GLOBAL_OFFSET_TABLE_` into `R_ARM_BASE_PREL`, whether it was written
    /// as a difference against a label or on its own, so that
    /// `.word _GLOBAL_OFFSET_TABLE_ - (1b + 8)` next to an `add rn, pc, rn`
    /// loads the GOT's address. Nothing else names the symbol that way: the
    /// `:lower16:` halves of it keep the `movw`/`movt` relocations.
    fn reloc_for_symbol(&self, reloc: u32, name: &str) -> u32 {
        if name == "_GLOBAL_OFFSET_TABLE_" && matches!(reloc, reloc::ABS32 | reloc::REL32) {
            reloc::BASE_PREL
        } else {
            reloc
        }
    }

    /// llvm-mc names a local label in every relocation but these two (its
    /// `ARMELFObjectWriter::needsRelocateWithSymbol`), and rsasm follows it:
    /// GNU as's `arm_fix_adjustable` instead folds the label's offset into
    /// the field and relocates against the section. A linker reads the two
    /// the same, and the whole-object corpora accept either, but a relocation
    /// under `REL` carries no addend of its own, so only the llvm-mc form
    /// says where it points without reading the field — which is what
    /// `tools/dwarf-diff` compares.
    fn relocates_with_label(&self, reloc: u32) -> bool {
        !matches!(reloc, reloc::ABS32 | reloc::PREL31)
    }

    /// llvm-mc's conventions, as for every ARM encoding, in either
    /// instruction set.
    fn dwarf(&self, _state: &ArchState, _format: crate::output::Format) -> DwarfTarget {
        DwarfTarget {
            cfi: Some(CfiTarget {
                data_align: -4,
                ra_column: 14,
                initial: vec![cfi::Insn::DefCfa(13, 0)],
                fde_encoding: 0x1b,
                eh_frame_align: 4,
                cie_version: 1,
            }),
            ..DwarfTarget::lines_only(Flavor::Llvm, 1)
        }
    }

    /// The AAPCS DWARF numbering of the names llvm-mc accepts: the core
    /// registers under their numbers, APCS names and aliases, and the 64-bit
    /// VFP registers from 256. `fp` is `r11` in both instruction sets.
    fn dwarf_register(
        &self,
        _state: &ArchState,
        _format: crate::output::Format,
        name: &str,
    ) -> Option<u32> {
        const APCS: [&str; 15] = [
            "a1", "a2", "a3", "a4", "v1", "v2", "v3", "v4", "v5", "v6", "v7", "v8", "ip", "sp",
            "lr",
        ];
        match name {
            "sb" => return Some(9),
            "sl" => return Some(10),
            "fp" => return Some(11),
            "pc" => return Some(15),
            _ => {}
        }
        if let Some(i) = APCS.iter().position(|r| *r == name) {
            return Some(i as u32);
        }
        numbered_register(name, "r", 15)
            .or_else(|| numbered_register(name, "d", 31).map(|n| 256 + n))
    }

    /// Alignment padding has to stay executable, and the two instruction sets
    /// have different no-ops, so the current mode picks.
    ///
    /// GNU as's `arm_handle_align`: a remainder too short for an instruction
    /// is zeros, and comes first so that the no-ops after it are aligned;
    /// then Thumb-2 padding is a 16-bit `nop` only where the rest is not a
    /// multiple of four, and 32-bit `nop.w`s for what is left, so that the
    /// processor fetches as few instructions as the gap allows.
    fn nop_fill(&self, state: &ArchState, len: u64) -> Vec<u8> {
        let len = len as usize;
        if state.bits == THUMB_BITS {
            let mut out = vec![0; len % 2];
            if !(len - out.len()).is_multiple_of(4) {
                out.extend_from_slice(&thumb::NOP.to_le_bytes());
            }
            while out.len() < len {
                out.extend_from_slice(&thumb::WIDE_NOP.0.to_le_bytes());
                out.extend_from_slice(&thumb::WIDE_NOP.1.to_le_bytes());
            }
            return out;
        }
        let nop = encode::NOP.to_le_bytes();
        let mut out = vec![0; len % nop.len()];
        while out.len() < len {
            out.extend_from_slice(&nop);
        }
        out
    }

    /// GNU as pads the end of a code section to its alignment, up to a word.
    fn pads_section_tail(&self, flags: &crate::section::SectionFlags) -> bool {
        flags.exec
    }

    fn section_tail_align_limit(&self) -> u64 {
        4
    }

    fn pads_as_last_instruction(&self) -> bool {
        true
    }

    /// `add_to_lit_pool` keeps an ARM pool as an array of four-byte slots in
    /// the order the literals were asked for, not a run per width.
    fn literal_pool(&self) -> crate::arch::LiteralPool {
        crate::arch::LiteralPool::Slots
    }

    /// Thumb branches, literal loads and `adr` are sized as GNU as's
    /// `arm_relax_frag` sizes them.
    fn relaxation(&self) -> crate::arch::Relaxation {
        crate::arch::Relaxation::EachPass
    }

    fn code_mapping(&self, state: &ArchState) -> Option<(&'static str, u64)> {
        Some(if state.bits == THUMB_BITS {
            ("$t", 2)
        } else {
            ("$a", 4)
        })
    }

    /// GNU as's `arm_frob_label`: a label remembers whether it was defined in
    /// Thumb code, and the first after `.thumb_func` in a code section, other
    /// than a `.L` local, is a Thumb function. A label is also where an `it`
    /// block the assembler made up ends, since it is a place a branch may
    /// land.
    fn label_flags(&self, state: &mut ArchState, name: &str, in_code: bool) -> u8 {
        thumb::close_implicit_it(state);
        let mut flags = 0;
        if state.bits == THUMB_BITS {
            flags |= LABEL_THUMB;
        }
        if state.private & PENDING_THUMB_FUNC != 0 && !name.starts_with(".L") && in_code {
            flags |= LABEL_THUMB_FUNC;
            state.private &= !PENDING_THUMB_FUNC;
        }
        flags
    }

    /// GNU as's `s_thumb_set`: `.thumb_set alias, value` is `.set` with the
    /// alias marked as a Thumb function, so that the symbol table holds its
    /// value with the low bit set and `STT_FUNC` — whatever the value was
    /// written as, and whichever instruction set the code around it is in.
    fn alias_directive(&self, name: &str) -> Option<u8> {
        (name == ".thumb_set").then_some(LABEL_THUMB | LABEL_THUMB_FUNC)
    }

    /// A Thumb function is `STT_FUNC`, and its address has the low bit set so
    /// that a call through it lands in Thumb state. Only a label defined in
    /// Thumb code is marked, though: GNU as marks nothing else, even a label
    /// `.thumb_func` names in ARM code.
    fn elf_symbol(&self, flags: u8, ty: SymType, defined: bool, value: u64) -> (SymType, u64) {
        if thumb_is_func(flags, ty) && flags & LABEL_THUMB != 0 {
            (SymType::Func, if defined { value | 1 } else { value })
        } else {
            (ty, value)
        }
    }

    /// GNU as's `arm_fix_adjustable`: a relocation against a function names
    /// it, so the linker knows which instruction set it is in.
    fn keeps_reloc_symbol(&self, flags: u8, ty: SymType) -> bool {
        ty == SymType::Func || thumb_is_func(flags, ty)
    }

    /// A linker sets the low bit of a Thumb function's address in data.
    fn link_bias(&self, reloc: u32, flags: u8, ty: SymType) -> i64 {
        i64::from((reloc == reloc::ABS32 || reloc == reloc::REL32) && thumb_is_func(flags, ty))
    }

    /// The branches between ARM and Thumb code, as GNU as assembles them and,
    /// for the references it leaves to a linker, as GNU ld links them.
    ///
    /// GNU as resolves a branch to a label in its own section that nothing
    /// outside the object can replace, and turns a call into the other
    /// instruction set into `blx`, or a `blx` that stays in its set into a
    /// call; a jump into the other set it leaves to the linker, which builds
    /// a veneer. A branch to a global symbol is always relocated. GNU ld then
    /// makes the same choice of `bl` or `blx` for the calls it resolves.
    fn interwork(&self, class: u8, t: &InterworkTarget) -> Interwork {
        let thumb = thumb_is_func(t.flags, t.ty);
        let arm = arm_is_func(t.flags, t.ty);
        // What the assembler resolves itself.
        let local = t.same_section && !t.global;
        let arm_blx = Interwork::Becomes {
            patch: encode::to_blx,
            kind: encode::blx_kind(),
        };
        let arm_bl = Interwork::Becomes {
            patch: encode::to_bl,
            kind: encode::bl_kind(),
        };
        let thumb_blx = Interwork::Becomes {
            patch: thumb::to_blx,
            kind: thumb::blx_kind(),
        };
        let thumb_bl = Interwork::Becomes {
            patch: thumb::to_bl,
            kind: if t.relocatable || local {
                thumb::blx_as_bl_kind()
            } else {
                thumb::bl_kind()
            },
        };
        // GNU as sets the low bit of a Thumb function's address in an `adr`
        // it relaxed once every symbol is known, which also makes it 32 bits.
        // `md_convert_frag` ORs the bit into the *addend*, not into the
        // finished `S + A - P`, so it adds one only where the addend is even;
        // which of the two classes the instruction carries says that, since
        // the addend is known when the `adr` is read. A `ldr rt, label`
        // shares the sizing but not the bit: `relax_adr` widens it for a
        // Thumb function, and the value it then writes is the plain address.
        match class {
            // `relax_adr` widens an `adr` of a weak symbol as it widens one
            // of a Thumb function, since it cannot know where either will end
            // up.
            IW_THUMB_ADR16 if thumb || t.weak => return Interwork::Relocate,
            // A PC-relative load resolves against a global symbol in its own
            // section, and a weak one leaves GNU as with a fixup it cannot
            // resolve and a field no relocation covers.
            IW_THUMB_LDR16 if thumb || t.weak => return Interwork::Relocate,
            IW_STRONG_ONLY if t.weak => return Interwork::Relocate,
            IW_THUMB_LDR16 | IW_STRONG_ONLY => return Interwork::AsWritten,
            // The widened `adr` is the one such reference GNU as writes for a
            // weak target instead of refusing it, and `arm_force_relocation`
            // then resolves it without the target: the field holds the addend
            // less the PC, plus the Thumb bit where the class says the addend
            // is even.
            IW_THUMB_ADR | IW_THUMB_ADR_ODD | IW_THUMB_ADR32 if t.weak => {
                let link = if thumb && class == IW_THUMB_ADR {
                    LinkValue::Split(|v| v + 1)
                } else {
                    LinkValue::Plain
                };
                return Interwork::Becomes {
                    patch: |w| w,
                    kind: FixupKind {
                        link,
                        ..thumb::adr32_kind().without_symbol()
                    },
                };
            }
            IW_THUMB_ADR if thumb => {
                return Interwork::Becomes {
                    patch: |w| w,
                    kind: FixupKind {
                        link: LinkValue::Split(|v| v + 1),
                        ..thumb::adr32_kind()
                    },
                };
            }
            IW_THUMB_ADR_ODD if thumb => {
                return Interwork::Becomes {
                    patch: |w| w,
                    kind: thumb::adr32_kind(),
                };
            }
            IW_THUMB_ADR16 | IW_THUMB_ADR | IW_THUMB_ADR_ODD | IW_THUMB_ADR32 => {
                return Interwork::AsWritten;
            }
            _ => {}
        }
        // A 16-bit branch has no relocation, so where the target is not
        // certain to stay put, GNU as takes the 32-bit form.
        if class == IW_THUMB_JUMP16 {
            return if arm || t.preemptible {
                Interwork::Relocate
            } else {
                Interwork::AsWritten
            };
        }
        if t.relocatable || local {
            return match class {
                IW_ARM_BL if local && thumb => arm_blx,
                IW_ARM_BLX if local && arm => arm_bl,
                IW_THUMB_BL if local && arm => thumb_blx,
                IW_THUMB_BLX if local && thumb => thumb_bl,
                IW_ARM_JUMP if thumb => Interwork::Relocate,
                IW_THUMB_JUMP if arm => Interwork::Relocate,
                // A branch to a global symbol is the core's to leave to the
                // linker; see `defers_to_linker`.
                _ => Interwork::AsWritten,
            };
        }
        // What GNU ld does with the relocation.
        match class {
            IW_ARM_BL if thumb => arm_blx,
            // A `blx` against a label reaches the linker as its section
            // symbol, which it takes to be ARM code.
            IW_ARM_BLX if !thumb && (arm || !t.global) => arm_bl,
            IW_THUMB_BL if arm => thumb_blx,
            IW_THUMB_BLX if thumb => thumb_bl,
            IW_ARM_JUMP if thumb => Interwork::LinkerOnly("an ARM-to-Thumb veneer"),
            IW_THUMB_JUMP if arm => Interwork::LinkerOnly("a Thumb-to-ARM veneer"),
            _ => Interwork::AsWritten,
        }
    }

    fn assemble(&self, cx: &mut AsmCtx<'_>, req: &InsnRequest<'_>) -> Option<Vec<Variant>> {
        let text = cx.name(req.mnemonic).to_ascii_lowercase();
        let Some(r) = insn::resolve(&text) else {
            cx.error(req.mnemonic_span, format!("unknown instruction `{text}`"));
            return None;
        };
        // What the target needs for a mnemonic a hand-written encoder owns,
        // from that mnemonic's own `insns[]` row. A form of the generated
        // table carries its own feature set and `generic::assemble` reads
        // that, which is finer: one row there is one element size.
        if !matches!(r.mnem, Mnem::Ext(_)) {
            let thumb = cx.state.bits == THUMB_BITS;
            let feats = cpu::mnemonic_feats(r.stem, thumb);
            if !cpu::supports(cx.state, feats) {
                let msg = cpu::unsupported(cx.state, &text, feats, thumb);
                cx.error(req.mnemonic_span, msg);
                return None;
            }
        }
        let ops = {
            let mut cur = req.cursor();
            // Only a branch target reads a `(plt)` suffix; see
            // `operand::Parser::suffixes`.
            let suffixes = match r.mnem {
                Mnem::B | Mnem::Bl | Mnem::Blx => BRANCH_SUFFIXES,
                _ => &[],
            };
            let mut p = operand::Parser { cx, suffixes };
            p.parse_list(&mut cur)?
        };
        // `!` requests writeback, which only the block transfers' base
        // register has, along with `srs` and `rfe` from the table; anywhere
        // else it would be silently meaningless.
        let takes_writeback = matches!(r.mnem, Mnem::Ldm(_) | Mnem::Stm(_) | Mnem::Ext(_));
        if let Some(op) = ops
            .iter()
            .enumerate()
            .find_map(|(i, op)| (op.writeback && !(takes_writeback && i == 0)).then_some(op))
        {
            cx.error(
                op.span,
                "`!` (writeback) is only valid on the base register of `ldm`/`stm`",
            );
            return None;
        }
        let ins = Insn {
            in_it: false,
            mnem: r.mnem,
            cond: r.cond,
            cond_written: r.cond_written,
            set_flags: r.set_flags,
            width: r.width,
            text: &text,
            ops: &ops,
            span: req.span,
        };
        if cx.state.bits == THUMB_BITS {
            let out = thumb::assemble(cx, &ins)?;
            // A 32-bit Thumb encoding needs Thumb-2 unless the mnemonic has
            // never had a narrow one; see `cpu::wide_ok`. The narrowest
            // encoding decides, since a mnemonic with a 16-bit one to fall
            // back on takes it.
            let narrowest = out.iter().map(|v| v.bytes.len()).min().unwrap_or(0);
            let always_wide = matches!(r.mnem, Mnem::Bl | Mnem::Blx);
            if narrowest > 2
                && !cpu::wide_ok(cx.state, cpu::mnemonic_feats(r.stem, true), always_wide)
            {
                let what = cpu::selected_name(cx.state);
                cx.error(
                    req.mnemonic_span,
                    format!("`{text}` needs a 32-bit Thumb encoding, which {what} has not"),
                );
                return None;
            }
            return Some(out);
        }
        arm_outside_it(cx, &ins);
        encode::assemble(cx, &ins)
    }

    fn directive(&self, cx: &mut AsmCtx<'_>, name: &str, cur: &mut Cursor<'_>) -> bool {
        match name {
            ".arm" | ".code32" => {
                set_mode(cx, false, cur.peek().span);
                true
            }
            ".thumb" | ".code16" => {
                set_mode(cx, true, cur.peek().span);
                true
            }
            ".code" => {
                // `.code 16` / `.code 32`, the spelling ARM sources use.
                let span = cur.peek().span;
                match cur.peek().kind {
                    TokKind::Int(16) => set_mode(cx, true, span),
                    TokKind::Int(32) => set_mode(cx, false, span),
                    _ => {
                        let span = cur.peek().span;
                        cx.error(span, "`.code` expects 16 or 32");
                        return true;
                    }
                }
                cur.advance();
                true
            }
            ".ltorg" | ".pool" => {
                cx.requests.push(Request::FlushLiterals);
                true
            }
            // The label after `.thumb_func` is a Thumb function; see
            // `label_flags`. The directive also switches to Thumb.
            ".thumb_func" => {
                set_mode(cx, true, cur.peek().span);
                cx.state.private |= PENDING_THUMB_FUNC;
                true
            }
            // Unified syntax is the only syntax this backend implements.
            ".syntax" => {
                cur.set_pos(cur.all().len());
                true
            }
            ".fpu" | ".arch_extension" | ".object_arch" => {
                let span = cur.peek().span;
                let Some(arg) = word(cx, cur) else {
                    cx.error(span, format!("`{name}` expects a name"));
                    return true;
                };
                let ok = match name {
                    ".fpu" => attrs::set_fpu(cx.state, &arg) | cpu::directive_fpu(cx.state, &arg),
                    ".arch_extension" => {
                        let attr = attrs::set_extension(cx.state, &arg);
                        match cpu::directive_extension(cx.state, &arg) {
                            Ok(known) => known | attr,
                            Err(msg) => {
                                cx.error(span, msg);
                                return true;
                            }
                        }
                    }
                    _ => attrs::set_object_arch(cx.state, &arg),
                };
                if !ok {
                    cx.error(span, format!("`{name}` does not know `{arg}`"));
                }
                true
            }
            ".eabi_attribute" => {
                eabi_attribute(cx, cur);
                true
            }
            ".tlsdescseq" => {
                tls_descseq(cx, cur);
                true
            }
            _ => false,
        }
    }
}

/// Runs the `it` state machine over an A32 instruction, which GNU as does so
/// that source written for both instruction sets can be told where Thumb
/// would need a block.
///
/// An A32 instruction carries its own condition and needs no block, so
/// `handle_pred_state` reports one outside a block with `as_tsktsk` rather
/// than refusing the line, and only in the two modes that do not accept it;
/// see [`crate::assembler::ImplicitIt`]. A written `it` emits nothing here (see
/// [`encode::assemble`]) but still opens a block, and an instruction it
/// covers says nothing. The conditions GNU as checks against the block are a
/// separate matter, which this backend does not check in either instruction
/// set.
fn arm_outside_it(cx: &mut AsmCtx<'_>, ins: &Insn<'_>) {
    let it = thumb::itstate(cx);
    if it & 0xf != 0 {
        thumb::advance_itstate(cx);
        return;
    }
    if ins.cond_written && ins.cond != AL && !cx.implicit_it.allows_arm() {
        cx.diags.warning(
            ins.span,
            format!(
                "`{}` is conditional outside an `it` block, which Thumb would not allow",
                ins.text
            ),
        );
    }
}

/// The rest of a word, as GNU as reads a CPU or unit name: `neon-vfpv4` and
/// `armv8.1-m.main` are several tokens each, and the name runs to the next
/// space. Lowercased, since GNU as matches these names case-insensitively.
fn word(cx: &AsmCtx<'_>, cur: &mut Cursor<'_>) -> Option<String> {
    if cur.peek().is_eol() {
        return None;
    }
    let first = cur.peek();
    let mut last = cur.advance();
    // A comma ends it: `.eabi_attribute Tag_ABI_align8_needed, 1` writes one
    // with nothing in between, and no name of GNU as's holds a comma.
    while !cur.peek().is_eol()
        && !cur.peek().preceded_by_space
        && !cur.peek().is_punct(Punct::Comma)
    {
        last = cur.advance();
    }
    Some(
        cx.sources
            .span_text(first.span.to(last.span))
            .to_ascii_lowercase(),
    )
}

/// `.eabi_attribute <tag>, <value>`: the tag is a number or one of the names
/// GNU as knows, and the value a number or a string. What it says replaces
/// whatever the CPU and its unit gave that tag; see [`attrs`].
fn eabi_attribute(cx: &mut AsmCtx<'_>, cur: &mut Cursor<'_>) {
    let span = cur.peek().span;
    let tag = match cur.peek().kind {
        TokKind::Int(n) if n <= u64::from(u32::MAX) => {
            cur.advance();
            n as u32
        }
        _ => {
            let Some(name) = word(cx, cur) else {
                cx.error(span, "`.eabi_attribute` expects a tag");
                return;
            };
            match attr_data::TAG_NAMES.iter().find(|&&(n, _)| n == name) {
                Some(&(_, tag)) => tag,
                None => {
                    cx.error(span, format!("`.eabi_attribute` does not know `{name}`"));
                    return;
                }
            }
        }
    };
    if !cur.peek().is_punct(Punct::Comma) {
        let span = cur.peek().span;
        cx.error(span, "`.eabi_attribute` expects a comma and a value");
        return;
    }
    cur.advance();
    let span = cur.peek().span;
    // The EABI's string tags: the two CPU names, `Tag_compatibility`,
    // `Tag_also_compatible_with` and `Tag_conformance`. GNU as refuses the
    // other spelling for either kind.
    let wants_string = matches!(tag, 4 | 5 | 32 | 65 | 67);
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
            cx.error(span, format!("`.eabi_attribute` tag {tag} takes a string"));
            cur.set_pos(cur.all().len());
            return;
        }
        _ => {
            cx.error(span, format!("`.eabi_attribute` tag {tag} takes a number"));
            cur.set_pos(cur.all().len());
            return;
        }
    };
    cx.requests.push(Request::Attribute {
        vendor: "aeabi",
        tag,
        value,
    });
}

/// `.tlsdescseq sym`: marks the instruction after it as part of a TLS
/// descriptor sequence, with `R_ARM_TLS_DESCSEQ` in ARM code and
/// `R_ARM_THM_TLS_DESCSEQ` in Thumb, which lets a linker that relaxes the
/// access rewrite it. The relocation covers no bits of the instruction.
///
/// GNU as's `s_arm_tls_descseq` places the relocation where the section has
/// got to, as a four-byte field that the next four bytes have to fill, and
/// calls `md_cons_align` first, which marks data there for the mapping
/// symbols; see [`Request::Mark`]. An addend is accepted and dropped, as it
/// is there. A number, which GNU as crashes on, is refused.
fn tls_descseq(cx: &mut AsmCtx<'_>, cur: &mut Cursor<'_>) {
    let span = cur.peek().span;
    let Some(e) = cx.expr_parser().parse(cur) else {
        return;
    };
    if cx.constant(e).is_some() {
        cx.error(span, "`.tlsdescseq` expects a symbol");
        return;
    }
    let name = cx.interner.intern("tlsdescseq");
    let espan = cx.exprs.span(e);
    let expr = cx
        .exprs
        .alloc(crate::expr::ExprKind::Modifier(name, e), espan);
    let reloc = if cx.state.bits == THUMB_BITS {
        reloc::THM_TLS_DESCSEQ
    } else {
        reloc::TLS_DESCSEQ
    };
    cx.requests.push(Request::Mark {
        expr,
        kind: FixupKind::data(0).with_reloc(reloc).linker_only(),
        as_data: true,
        within: 4,
    });
}

/// Switches between ARM and Thumb, as GNU as's `.arm` and `.thumb` do: the
/// section's alignment is raised to two bytes, and ARM code after Thumb
/// starts on a word boundary, padded with zeros rather than no-ops.
///
/// An `it` block the assembler made up also ends here; see
/// [`thumb::close_implicit_it`].
fn set_mode(cx: &mut AsmCtx<'_>, thumb: bool, span: crate::source::Span) {
    if (cx.state.bits == THUMB_BITS) == thumb {
        return;
    }
    // GNU as's `opcode_select` refuses the switch where the target has no
    // such instruction set: an M-profile core has no ARM state, and a core
    // older than ARMv4T no Thumb.
    let (want, what) = if thumb {
        (cpu_data::V4T, "Thumb")
    } else {
        (cpu_data::V1, "ARM")
    };
    if !cpu::has(cx.state, want) {
        let name = cpu::selected_name(cx.state);
        cx.error(span, format!("{name} has no {what} instruction set"));
        return;
    }
    thumb::close_implicit_it(cx.state);
    cx.state.bits = if thumb { THUMB_BITS } else { 32 };
    if !thumb {
        cx.requests.push(Request::AlignZero(4));
    }
    cx.requests.push(Request::RecordAlign(2));
}

/// True if `name` is an ARM register, for callers that need to avoid treating
/// register names as symbols.
#[allow(dead_code)]
pub fn is_register(name: &str) -> bool {
    reg::is_register(name)
}
