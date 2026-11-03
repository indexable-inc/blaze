//! What a relocation computes, in terms no object format owns.
//!
//! A backend builds fixups knowing what each field means — a branch
//! displacement, the page of a symbol, the slot a GOT load reads — but it has
//! to name that meaning somehow, and until there was one object format the
//! name was simply the ELF relocation number. Mach-O numbers the same
//! meanings differently, splits some of them by how many bytes follow the
//! field, and has no number at all for others, so the number cannot be the
//! description.
//!
//! [`RelocClass`] is that description. A fixup carries one (see
//! [`FixupKind::class`](crate::section::FixupKind::class)) alongside its ELF
//! number, the layout pass records it on every relocation it builds, and each
//! writer maps it to its own numbering: the ELF writer still writes the number
//! the backend chose, and the Mach-O writer maps the class. Only the classes a
//! format writes differently from a plain reference need a name of their own;
//! everything else is [`RelocClass::Plain`], which the fixup's width and
//! `pcrel` flag already describe.

use crate::section::FixupKind;
use crate::symbol::SymbolId;

/// What a relocation's target contributes to the field.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum RelocClass {
    /// The target itself: `S + A`, or `S + A - P` where the fixup is
    /// PC-relative.
    #[default]
    Plain,
    /// A call or jump displacement. A linker may route it through a stub, so
    /// Mach-O gives it a relocation of its own (`X86_64_RELOC_BRANCH`,
    /// `ARM64_RELOC_BRANCH26`) rather than the plain PC-relative one.
    Branch,
    /// The address of the symbol's GOT slot.
    Got,
    /// The same, in a load a linker may rewrite into a direct reference:
    /// x86-64's `movq foo@GOTPCREL(%rip), %reg`.
    GotLoad,
    /// The page of the target, relative to the page of the field: AArch64's
    /// `adrp`.
    Page,
    /// The low twelve bits of the target, completing a [`RelocClass::Page`].
    PageOff,
    /// The page of the symbol's GOT slot.
    GotPage,
    /// The low twelve bits of the symbol's GOT slot.
    GotPageOff,
    /// One 16-bit group of the target, which an instruction holding sixteen
    /// bits of an address takes: AArch64's `movz`/`movk`/`movn` groups.
    /// Mach-O has no relocation for one.
    AddressGroup,
    /// Part of an ELF thread-local access sequence, whose value only the
    /// linker can work out and which it may rewrite into another model:
    /// AArch64's `:tprel_lo12:`, `:tlsdesc:` and the rest. Neither Mach-O nor
    /// COFF has a relocation for one. A field of no width is a mark on an
    /// instruction of such a sequence (`.tlsdesccall`) rather than an access
    /// of its own.
    ThreadLocal,
    /// The address of a thread-local variable's descriptor, which Darwin's
    /// loader fills in for each thread: `sym@TLVP` on x86-64, and
    /// `sym@TLVPPAGE` with `sym@TLVPPAGEOFF` on arm64. Only Mach-O has one;
    /// ELF names a place in the thread's block instead, which is
    /// [`RelocClass::ThreadLocal`].
    ThreadVariable,
    /// The page of that descriptor, relative to the page of the field:
    /// `sym@TLVPPAGE`.
    ThreadVariablePage,
    /// The low twelve bits of it, completing a
    /// [`RelocClass::ThreadVariablePage`].
    ThreadVariablePageOff,
    /// The target itself, in a 32-bit field the CPU sign-extends to 64 bits:
    /// an x86-64 displacement, or a 64-bit operation's immediate. ELF calls it
    /// `R_X86_64_32S`; Mach-O has no relocation for it.
    SignExtended,
    /// The target's address relative to the base the image is loaded at,
    /// which only a PE image has: COFF's `.rva` and `@IMGREL`.
    ImageRelative,
    /// The target's offset within its own section: COFF's `.secrel32`.
    SectionRelative,
    /// The one-based index of the target's section: COFF's `.secidx`.
    SectionIndex,
    /// A symbol an FDE points at: the function it describes, or its
    /// language-specific data area.
    ///
    /// ELF and COFF relocate these like any other reference, so the class
    /// says nothing there. A Mach-O object gives every section an address,
    /// and on x86-64 llvm-mc uses that to write the distance itself rather
    /// than leave it to the linker
    /// (`MCAsmInfo::DwarfFDESymbolsUseAbsDiff`), which is why the field
    /// needs a name of its own.
    FrameSymbol,
}

/// The format-neutral description of one relocation, recorded next to the
/// ELF number on [`Relocation`](crate::assembler::Relocation).
#[derive(Copy, Clone, Debug)]
pub struct RelocDesc {
    pub class: RelocClass,
    /// Field width in bytes.
    pub size: u8,
    pub pcrel: bool,
    /// Bytes of the instruction after the field. Mach-O's
    /// `X86_64_RELOC_SIGNED_1/2/4` exist because its PC-relative relocations
    /// are defined from the end of the field rather than the end of the
    /// instruction, so the difference has to be recorded in the type.
    ///
    /// Negative where the reference is measured from the *start* of the
    /// field, as DWARF's `DW_EH_PE_pcrel` is: there the relocation reaches
    /// four bytes further than the value does, and the field makes up the
    /// difference.
    pub trailing: i8,
    /// The symbol subtracted from the target, for a difference that only a
    /// pair of relocations can express (Mach-O's `SUBTRACTOR`).
    pub subtrahend: Option<SymbolId>,
}

impl RelocDesc {
    /// The description of the relocation a fixup asks for, before any
    /// modifier or pairing is taken into account.
    pub fn of(kind: &FixupKind) -> RelocDesc {
        // A PC-relative field measured from `adjust` bytes past its start
        // has the difference between that and its width after it.
        let trailing = if kind.pcrel {
            (kind.adjust as i16 - kind.size as i16).clamp(-128, 127) as i8
        } else {
            0
        };
        RelocDesc {
            class: kind.class,
            size: kind.size,
            pcrel: kind.pcrel,
            trailing,
            subtrahend: None,
        }
    }
}
