//! Mach-O relocatable object output (`MH_OBJECT`), for x86-64, i386 and
//! arm64.
//!
//! Mach-O is not ELF with other numbers. Three things about it shape both
//! this writer and the few decisions the layout pass has to make differently
//! (all of them behind [`Format::MachO`](crate::output::Format::MachO)):
//!
//! * **Sections belong to segments, and a section is named by the pair.**
//!   `.text` is `__TEXT,__text`, and the source can name any pair with
//!   `.section __DATA,__foo`. All sections of an object share one nameless
//!   segment and one address space, so every section has an address here,
//!   unlike in a relocatable ELF object where each starts at zero.
//!
//! * **A relocation has no addend field.** What ELF puts in `r_addend` goes
//!   into the field being relocated, biased as each relocation type expects,
//!   and a difference of two symbols needs a `SUBTRACTOR`/`UNSIGNED` pair
//!   because one entry can only add. On arm64, where the fields are scattered
//!   through the instruction word and cannot hold an addend at all, the addend
//!   is a relocation of its own (`ARM64_RELOC_ADDEND`).
//!
//! * **Code is made of atoms.** A label whose name does not start with `L` is
//!   a *linker-visible* symbol, and the linker may move the code from it up to
//!   the next such label independently of everything around it. A reference
//!   that crosses from one atom into another therefore cannot be resolved
//!   here, however close the two are and whether or not the target is global;
//!   it is relocated against the target's atom, with the distance from the
//!   atom carried as the addend. That is what the `Atoms` table below is for,
//!   and it is why a Mach-O object has relocations where an ELF one has none.
//!
//! What is written: one `LC_SEGMENT_64` with every section; the deployment
//! target where the target triple or the source gave one, as
//! `LC_BUILD_VERSION` or the older `LC_VERSION_MIN_*`;
//! `LC_DATA_IN_CODE` where it marked data in code;
//! `LC_SYMTAB` with `LC_DYSYMTAB`, whose three-way split of the symbol table
//! is required, not optional, unless there are no symbols at all, and which
//! points at the indirect symbol table `.indirect_symbol` fills; and one
//! `LC_LINKER_OPTION` per `.linker_option`.
//!
//! As far as llvm-mc 22 writes the same object, rsasm writes it byte for
//! byte, down to the order of the symbols and the string table's shared
//! tails; `tools/macho-diff` checks that.
//!
//! The debugging sections that `-g`, `.loc` and `.cfi_*` make are written here
//! as well, in a `__DWARF` segment and in `__TEXT,__eh_frame`. Because every
//! section already has an address, most of what an ELF object relocates is a
//! number here; see the `dwarf` module. A frame is also described by one word
//! in `__LD,__compact_unwind`, which the linker reads in preference to the
//! frame table: always on arm64, and on x86-64 where the deployment target
//! is a macOS whose linker read one; see `CompactUnwind` and `Deployment`.

mod directives;
mod relocations;

use super::OutputError;
use crate::assembler::{Assembler, Relocation};
use crate::reloc::RelocClass;
use crate::section::SectionId;
use crate::symbol::{Binding, SymbolId, SymbolValue, Visibility};
use std::collections::{HashMap, HashSet};

// ---- the format's constants -------------------------------------------------

const MH_MAGIC: u32 = 0xfeed_face;
const MH_MAGIC_64: u32 = 0xfeed_facf;
const MH_OBJECT: u32 = 1;
const MH_SUBSECTIONS_VIA_SYMBOLS: u32 = 0x2000;

/// `CPU_ARCH_ABI64`, the bit a machine's 64-bit variant carries.
const CPU_ARCH_ABI64: u32 = 0x0100_0000;
const CPU_TYPE_X86: u32 = 7;
const CPU_TYPE_X86_64: u32 = CPU_ARCH_ABI64 | CPU_TYPE_X86;
const CPU_SUBTYPE_I386_ALL: u32 = 3;
const CPU_SUBTYPE_X86_64_ALL: u32 = 3;
const CPU_TYPE_ARM64: u32 = 0x0100_000c;
const CPU_SUBTYPE_ARM64_ALL: u32 = 0;

const LC_SYMTAB: u32 = 0x2;
const LC_DYSYMTAB: u32 = 0xb;
const LC_SEGMENT: u32 = 0x1;
const LC_SEGMENT_64: u32 = 0x19;
pub(crate) const LC_VERSION_MIN_MACOSX: u32 = 0x24;
pub(crate) const LC_VERSION_MIN_IPHONEOS: u32 = 0x25;
const LC_DATA_IN_CODE: u32 = 0x29;
pub(crate) const LC_LINKER_OPTION: u32 = 0x2d;
pub(crate) const LC_VERSION_MIN_TVOS: u32 = 0x2f;
pub(crate) const LC_VERSION_MIN_WATCHOS: u32 = 0x30;
pub(crate) const LC_BUILD_VERSION: u32 = 0x32;

/// `PLATFORM_MACOS`, the only platform `LC_BUILD_VERSION` is written for
/// here; see [`Deployment`].
pub(crate) const PLATFORM_MACOS: u32 = 1;

const SEGMENT_COMMAND_SIZE: u32 = 56;
const SEGMENT_COMMAND_64_SIZE: u32 = 72;
const SECTION_SIZE: u32 = 68;
const SECTION_64_SIZE: u32 = 80;
const SYMTAB_COMMAND_SIZE: u32 = 24;
const DYSYMTAB_COMMAND_SIZE: u32 = 80;
const BUILD_VERSION_COMMAND_SIZE: u32 = 24;
const VERSION_MIN_COMMAND_SIZE: u32 = 16;
const LINKER_OPTION_COMMAND_SIZE: u32 = 12;
const INDIRECT_SYMBOL_SIZE: u64 = 4;
const LINKEDIT_DATA_COMMAND_SIZE: u32 = 16;
const DATA_IN_CODE_ENTRY_SIZE: u64 = 8;
const HEADER_SIZE: u32 = 28;
const HEADER_64_SIZE: u32 = 32;
const NLIST_SIZE: u64 = 12;
const NLIST_64_SIZE: u64 = 16;
const RELOCATION_SIZE: u64 = 8;

/// `R_SCATTERED`, the top bit of a relocation's first word, which says the
/// record is a `scattered_relocation_info` rather than a `relocation_info`;
/// see [`Entry`].
const R_SCATTERED: u32 = 0x8000_0000;

// Section types (the low byte of `flags`).
pub(crate) const S_REGULAR: u32 = 0x0;
pub(crate) const S_ZEROFILL: u32 = 0x1;
pub(crate) const S_CSTRING_LITERALS: u32 = 0x2;
pub(crate) const S_4BYTE_LITERALS: u32 = 0x3;
pub(crate) const S_8BYTE_LITERALS: u32 = 0x4;
pub(crate) const S_LITERAL_POINTERS: u32 = 0x5;
pub(crate) const S_NON_LAZY_SYMBOL_POINTERS: u32 = 0x6;
pub(crate) const S_LAZY_SYMBOL_POINTERS: u32 = 0x7;
pub(crate) const S_SYMBOL_STUBS: u32 = 0x8;
pub(crate) const S_MOD_INIT_FUNC_POINTERS: u32 = 0x9;
pub(crate) const S_MOD_TERM_FUNC_POINTERS: u32 = 0xa;
pub(crate) const S_COALESCED: u32 = 0xb;
pub(crate) const S_GB_ZEROFILL: u32 = 0xc;
pub(crate) const S_INTERPOSING: u32 = 0xd;
pub(crate) const S_16BYTE_LITERALS: u32 = 0xe;
pub(crate) const S_THREAD_LOCAL_REGULAR: u32 = 0x11;
pub(crate) const S_THREAD_LOCAL_ZEROFILL: u32 = 0x12;
pub(crate) const S_THREAD_LOCAL_VARIABLES: u32 = 0x13;
pub(crate) const S_THREAD_LOCAL_VARIABLE_POINTERS: u32 = 0x14;
pub(crate) const S_THREAD_LOCAL_INIT_FUNCTION_POINTERS: u32 = 0x15;

pub(crate) const S_ATTR_PURE_INSTRUCTIONS: u32 = 0x8000_0000;
pub(crate) const S_ATTR_NO_TOC: u32 = 0x4000_0000;
pub(crate) const S_ATTR_STRIP_STATIC_SYMS: u32 = 0x2000_0000;
pub(crate) const S_ATTR_NO_DEAD_STRIP: u32 = 0x1000_0000;
pub(crate) const S_ATTR_LIVE_SUPPORT: u32 = 0x0800_0000;
pub(crate) const S_ATTR_SELF_MODIFYING_CODE: u32 = 0x0400_0000;
pub(crate) const S_ATTR_DEBUG: u32 = 0x0200_0000;
pub(crate) const S_ATTR_SOME_INSTRUCTIONS: u32 = 0x0000_0400;

// `n_type`.
const N_UNDF: u8 = 0x0;
const N_ABS: u8 = 0x2;
const N_SECT: u8 = 0xe;
const N_EXT: u8 = 0x1;
const N_TYPE: u8 = 0xe;
const N_PEXT: u8 = 0x10;

// `n_desc` bits.
/// The low four bits of `n_desc` on an undefined symbol: the linker is to
/// bind it the first time it is used, which `.indirect_symbol` in a lazy
/// pointer or stub section asks for.
pub(crate) const REFERENCE_FLAG_UNDEFINED_LAZY: u16 = 0x0001;
pub(crate) const N_NO_DEAD_STRIP: u16 = 0x0020;
pub(crate) const N_WEAK_REF: u16 = 0x0040;
pub(crate) const N_WEAK_DEF: u16 = 0x0080;
pub(crate) const N_ALT_ENTRY: u16 = 0x0200;

// Relocation types, per machine.
mod x86_64_reloc {
    pub(crate) const UNSIGNED: u8 = 0;
    pub(crate) const SIGNED: u8 = 1;
    pub(crate) const BRANCH: u8 = 2;
    pub(crate) const GOT_LOAD: u8 = 3;
    pub(crate) const GOT: u8 = 4;
    pub(crate) const SUBTRACTOR: u8 = 5;
    pub(crate) const SIGNED_1: u8 = 6;
    pub(crate) const SIGNED_2: u8 = 7;
    pub(crate) const SIGNED_4: u8 = 8;
    pub(crate) const TLV: u8 = 9;
}

/// The relocation types a 32-bit Mach-O object has, which are the same on
/// every machine: `<mach-o/reloc.h>`'s `GENERIC_RELOC_*`. A machine adds its
/// own above them.
mod generic_reloc {
    pub(crate) const VANILLA: u8 = 0;
    pub(crate) const PAIR: u8 = 1;
    pub(crate) const SECTDIFF: u8 = 2;
    pub(crate) const LOCAL_SECTDIFF: u8 = 4;
    pub(crate) const TLV: u8 = 5;
}

mod arm64_reloc {
    pub(crate) const UNSIGNED: u8 = 0;
    pub(crate) const SUBTRACTOR: u8 = 1;
    pub(crate) const BRANCH26: u8 = 2;
    pub(crate) const PAGE21: u8 = 3;
    pub(crate) const PAGEOFF12: u8 = 4;
    pub(crate) const GOT_LOAD_PAGE21: u8 = 5;
    pub(crate) const GOT_LOAD_PAGEOFF12: u8 = 6;
    pub(crate) const POINTER_TO_GOT: u8 = 7;
    pub(crate) const TLVP_LOAD_PAGE21: u8 = 8;
    pub(crate) const TLVP_LOAD_PAGEOFF12: u8 = 9;
    pub(crate) const ADDEND: u8 = 10;
}

/// The machines this writer can produce objects for.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub(crate) enum Cpu {
    X86_64,
    I386,
    Arm64,
}

impl Cpu {
    /// The Mach-O machine an architecture backend targets, if it has one.
    pub(crate) fn for_arch(arch: &dyn crate::arch::Architecture) -> Option<Cpu> {
        match arch.elf_machine() {
            3 => Some(Cpu::I386),
            62 => Some(Cpu::X86_64),
            183 => Some(Cpu::Arm64),
            _ => None,
        }
    }

    fn header(self) -> (u32, u32) {
        match self {
            Cpu::X86_64 => (CPU_TYPE_X86_64, CPU_SUBTYPE_X86_64_ALL),
            Cpu::I386 => (CPU_TYPE_X86, CPU_SUBTYPE_I386_ALL),
            Cpu::Arm64 => (CPU_TYPE_ARM64, CPU_SUBTYPE_ARM64_ALL),
        }
    }

    /// Whether the object is a 64-bit Mach-O, whose header, segment command,
    /// section records and symbol table entries are all wider, and whose
    /// file is padded to eight bytes rather than four.
    fn wide(self) -> bool {
        self != Cpu::I386
    }

    /// Whether the machine relocates with Mach-O's generic relocations,
    /// which is what every 32-bit object does: one type for everything a
    /// field can hold by itself, and a scattered record for what it cannot.
    /// See [`Entry`] and [`scattered`].
    fn generic_relocs(self) -> bool {
        !self.wide()
    }

    /// Whether the machine's PC-relative relocations are trusted to carry a
    /// reference from one atom to another in the same section.
    ///
    /// Only x86-64's are: llvm-mc (`hasReliableSymbolDifference`) resolves
    /// such a reference on every other machine unless the file asks for its
    /// atoms to be kept apart; see [`defers_to_linker`].
    fn reliable_symbol_difference(self) -> bool {
        self == Cpu::X86_64
    }

    /// Whether every section is given a `ltmpN` label at its start, so that a
    /// relocation naming a position with no atom has a symbol to name.
    ///
    /// llvm-mc does this for arm64, whose relocations must all be external.
    fn labels_sections(self) -> bool {
        self == Cpu::Arm64
    }

    /// Whether the writer works out an FDE's `initial_location` itself rather
    /// than leaving the frame table a relocation.
    ///
    /// Every section of the object has an address here, so the distance from
    /// the field to the function is a number on x86, where llvm-mc writes one
    /// too. On arm64, where every relocation has to be external, llvm-mc
    /// writes the difference of the function and the field, which is a
    /// `SUBTRACTOR` pair.
    pub(crate) fn resolves_frame_address(self) -> bool {
        self != Cpu::Arm64
    }
}

/// How a Mach-O machine splits a frame between `__LD,__compact_unwind` and
/// `__TEXT,__eh_frame`.
///
/// Darwin's linker reads the compact word first and goes to the frame table
/// only where the word tells it to, so a frame described in one of the two
/// alone is described wrongly unless these say otherwise.
#[derive(Copy, Clone, Debug)]
pub(crate) struct CompactUnwind {
    /// The word that means the frame is described in the frame table
    /// (`MCObjectFileInfo::CompactUnwindDwarfEHFrameOnly`).
    pub(crate) dwarf_only: u32,
    /// Whether a frame whose word describes it needs no frame table entry
    /// at all (`getSupportsCompactUnwindWithoutEHFrame`).
    pub(crate) without_eh_frame: bool,
}

/// `UNWIND_HAS_LSDA`: the bit the compact word carries when the entry points
/// at a language-specific data area.
pub(crate) const UNWIND_HAS_LSDA: u32 = 0x4000_0000;

/// What a Darwin target triple says about an object beyond naming its
/// machine: which release of the system the object is for, and whether the
/// linker is given a compact unwind table.
///
/// llvm-mc settles both before it reads a line of source —
/// `MCStreamer::emitVersionForTarget` writes the load command and
/// `MCObjectFileInfo`'s `useCompactUnwind` decides on the table — which is
/// why a `.macosx_version_min` in the source changes the load command
/// without adding a table.
#[derive(Copy, Clone, Default, Debug)]
pub(crate) struct Deployment {
    /// The version load command the triple asks for, where it named a
    /// version; a directive in the source replaces it.
    pub(crate) version: Option<BuildVersion>,
    /// The compact unwind table the object has, where it has one; see
    /// [`CompactUnwind`].
    pub(crate) compact_unwind: Option<CompactUnwind>,
}

impl Deployment {
    /// What `triple` says about an object for `cpu`. The triple is the whole
    /// `-a` argument, since the machine is only its first component.
    pub(crate) fn of(cpu: Cpu, triple: Option<&str>) -> Deployment {
        // arm64 macOS begins at 11.0, and llvm-mc raises an older deployment
        // target to it rather than name a release no arm64 Mac ever ran
        // (`Triple::getMinimumSupportedOSVersion`).
        let floor = match cpu {
            Cpu::Arm64 => (11, 0, 0),
            _ => (0, 0, 0),
        };
        let macos = triple.and_then(macos_version).map(|v| v.max(floor));
        Deployment {
            version: macos.map(|(major, minor, patch)| BuildVersion {
                // `LC_BUILD_VERSION` names its platform and came in with the
                // releases that needed one; before macOS 10.14 llvm-mc
                // writes the command that does not.
                command: if (major, minor) >= (10, 14) {
                    LC_BUILD_VERSION
                } else {
                    LC_VERSION_MIN_MACOSX
                },
                platform: PLATFORM_MACOS,
                minos: (major << 16) | (minor << 8) | patch,
                sdk: 0,
            }),
            compact_unwind: match cpu {
                // llvm-mc writes the table for x86 only from macOS 10.6 on,
                // the release whose linker first read one, and leaves every
                // frame in the frame table as well, since the compact word
                // alone describes a frame only on arm64.
                Cpu::X86_64 | Cpu::I386 => {
                    macos
                        .is_some_and(|v| v >= (10, 6, 0))
                        .then_some(CompactUnwind {
                            dwarf_only: 0x0400_0000,
                            without_eh_frame: false,
                        })
                }
                Cpu::Arm64 => Some(CompactUnwind {
                    dwarf_only: 0x0300_0000,
                    without_eh_frame: true,
                }),
            },
        }
    }
}

/// The macOS version a Darwin target triple names, in the spellings llvm-mc
/// reads: `macos10.6` and `macosx14.1` name the release outright, and
/// `darwin10` the kernel that shipped with it.
///
/// A kernel version is translated as `Triple::getMacOSXVersion` translates
/// it, and only its major part is: macOS 10.(N-4) up to Darwin 19, N-9 from
/// Darwin 20, and N+1 from Darwin 25, which is where macOS skipped from 15 to
/// 26. A kernel older than Darwin 4 predates the table and is read as a
/// release number as it stands.
///
/// Only macOS is read. The other Darwin platforms have deployment targets
/// too, but a version for one of them is left out of the object rather than
/// guessed at.
fn macos_version(triple: &str) -> Option<(u32, u32, u32)> {
    let lower = triple.to_ascii_lowercase();
    let (os, version) = lower.split('-').find_map(|part| {
        ["macosx", "macos", "darwin"]
            .into_iter()
            .find_map(|os| part.strip_prefix(os).map(|rest| (os, rest)))
    })?;
    let mut parts = version.split('.').map(|n| n.parse::<u32>());
    let major = parts.next()?.ok()?;
    let minor = parts.next().unwrap_or(Ok(0)).ok()?;
    let patch = parts.next().unwrap_or(Ok(0)).ok()?;
    if parts.next().is_some() {
        return None;
    }
    // A triple whose major version is zero names no release, and llvm-mc
    // writes no version command for it.
    if major == 0 {
        return None;
    }
    Some(match (os, major) {
        ("darwin", 4..20) => (10, major - 4, 0),
        ("darwin", 20..25) => (major - 9, 0, 0),
        ("darwin", 25..) => (major + 1, 0, 0),
        _ => (major, minor, patch),
    })
}

// ---- assembler-side state ---------------------------------------------------

/// The deployment target a `.build_version` or a `.*_version_min` gave the
/// object. The last of them wins, as it does in llvm-mc, where both write the
/// same field.
#[derive(Copy, Clone, Debug)]
pub(crate) struct BuildVersion {
    /// `LC_BUILD_VERSION`, or the `LC_VERSION_MIN_*` of the platform the
    /// directive was named after.
    pub(crate) command: u32,
    /// The platform, which only `LC_BUILD_VERSION` names.
    pub(crate) platform: u32,
    /// Packed `xxxx.yy.zz`, as Mach-O stores versions.
    pub(crate) minos: u32,
    pub(crate) sdk: u32,
}

/// The flags an indirect symbol table entry carries in place of a symbol
/// index, for a pointer the linker can fill in without one.
const INDIRECT_SYMBOL_LOCAL: u32 = 0x8000_0000;
const INDIRECT_SYMBOL_ABS: u32 = 0x4000_0000;

/// What one section is in Mach-O's terms, as its directive declared it. Its
/// name holds the segment and section names; see [`split_name`].
#[derive(Clone, Debug)]
pub(crate) struct SectionInfo {
    /// The section type, the low byte of `flags`.
    pub(crate) ty: u32,
    /// `S_ATTR_*` bits.
    pub(crate) attrs: u32,
    /// The stub size of a `symbol_stubs` section.
    pub reserved2: u32,
}

/// A stretch of data in code, from `.data_region` to `.end_data_region`,
/// which `LC_DATA_IN_CODE` tells a disassembler not to decode.
#[derive(Clone, Debug)]
pub(crate) struct DataRegion {
    /// `DICE_KIND_*`: data, or a jump table of 8, 16 or 32-bit entries.
    pub(crate) kind: u16,
    pub(crate) start: SymbolId,
    pub(crate) end: Option<SymbolId>,
    pub(crate) span: crate::source::Span,
}

/// Everything the source told the assembler that only Mach-O output cares
/// about.
#[derive(Default)]
pub(crate) struct State {
    pub(crate) subsections_via_symbols: bool,
    /// What the target triple said; see [`Deployment`]. Set once, before any
    /// source is read, and only for Mach-O output.
    pub(crate) deployment: Deployment,
    pub(crate) build_version: Option<BuildVersion>,
    pub(crate) sections: HashMap<SectionId, SectionInfo>,
    /// `n_desc` bits from `.weak_definition`, `.weak_reference`,
    /// `.alt_entry` and `.no_dead_strip`.
    pub(crate) symbol_desc: HashMap<SymbolId, u16>,
    /// The symbols `.set` or `.equ` defined, as opposed to `=`.
    pub(crate) set_constants: HashSet<SymbolId>,
    /// The data regions, in the order they were opened.
    pub(crate) data_regions: Vec<DataRegion>,
    /// How many symbols there were when each section was created, which is
    /// where its arm64 section label goes among them.
    pub(crate) section_marks: HashMap<SectionId, u32>,
    /// What each `.indirect_symbol` named, in the order they were written:
    /// the pointer or stub section it was in, and the name.
    pub(crate) indirect_symbols: Vec<(SectionId, crate::intern::Name)>,
    /// The same with the symbols interned, which only happens once the source
    /// has been read. Their order is the indirect symbol table's, and where a
    /// section's first entry falls in it is that section's `reserved1`.
    ///
    /// llvm-mc creates these symbols last of all
    /// (`MachObjectWriter::bindIndirectSymbols`), so one that nothing else in
    /// the file mentions comes after every label in the symbol table rather
    /// than where the directive was.
    pub(crate) indirect_bound: Vec<(SectionId, SymbolId)>,
    /// One `LC_LINKER_OPTION` per `.linker_option`, each the words it gave.
    pub(crate) linker_options: Vec<Vec<String>>,
    /// Where every atom starts, once the source has been read; see [`Atoms`].
    pub(crate) atoms: Atoms,
}

impl State {
    pub(crate) fn desc(&self, id: SymbolId) -> u16 {
        self.symbol_desc.get(&id).copied().unwrap_or(0)
    }
}

// ---- atoms ------------------------------------------------------------------

/// Where each atom of each section begins.
///
/// An atom starts at every linker-visible label — one whose name does not
/// start with `L` — and runs to the next. Positions are kept as fragment
/// indices rather than addresses so that the table stays valid while layout is
/// still moving things about, together with the order each label was defined
/// in: of several labels at one position, one defined before the
/// linker-visible label still ends the atom before it, as it does in llvm-mc,
/// which starts a fragment at every linker-visible label.
#[derive(Default)]
pub(crate) struct Atoms {
    /// Per section, `(fragment, definition order, symbol)`, in that order.
    starts: HashMap<SectionId, Vec<(u32, u32, SymbolId)>>,
}

impl Atoms {
    /// The symbol whose atom covers fragment `frag` of `section`, if any.
    /// Every label at that fragment counts as before it.
    pub(crate) fn at(&self, section: SectionId, frag: u32) -> Option<SymbolId> {
        self.before(section, frag, u32::MAX)
    }

    /// The last linker-visible label at or before `(frag, order)`.
    fn before(&self, section: SectionId, frag: u32, order: u32) -> Option<SymbolId> {
        let list = self.starts.get(&section)?;
        let i = list.partition_point(|&(f, o, _)| (f, o) <= (frag, order));
        (i > 0).then(|| list[i - 1].2)
    }

    /// The atom a symbol belongs to: itself when it is linker-visible.
    pub(crate) fn of(&self, asm: &Assembler, id: SymbolId) -> Option<SymbolId> {
        let sym = asm.symbols.get(id);
        if !is_temporary(asm.interner.get(sym.name)) {
            return sym.is_defined().then_some(id);
        }
        let SymbolValue::Label { section, frag } = sym.value else {
            return None;
        };
        // A literal section is cut into atoms by its contents, not by labels,
        // so a label in one names no atom.
        atomizable(asm, section).then(|| self.before(section, frag, sym.def_order))?
    }
}

/// Collects the atom starts of every section. Called once the source has been
/// read, before layout resolves anything.
pub(crate) fn atoms(asm: &Assembler) -> Atoms {
    let mut starts: HashMap<SectionId, Vec<(u32, u32, SymbolId)>> = HashMap::new();
    for (id, sym) in asm.symbols.iter() {
        let SymbolValue::Label { section, frag } = sym.value else {
            continue;
        };
        if is_temporary(asm.interner.get(sym.name)) {
            continue;
        }
        starts
            .entry(section)
            .or_default()
            .push((frag, sym.def_order, id));
    }
    for list in starts.values_mut() {
        list.sort_by_key(|&(f, o, _)| (f, o));
    }
    Atoms { starts }
}

/// Whether a symbol is assembler-local, which in Mach-O is decided by the
/// name alone: Darwin's private label prefix is `L`. rsasm's own made-up
/// labels carry a NUL, which no source can spell, and are local too.
pub(crate) fn is_temporary(name: &str) -> bool {
    name.starts_with('L') || name.contains('\u{0}')
}

/// Whether a section is cut into atoms by the labels in it.
///
/// A literal section is not: its contents are cut up and merged by the linker
/// item by item, so a reference into one has to name the label it refers to
/// rather than a position. This is `MCAsmInfoDarwin::isSectionAtomizableBySymbols`.
pub(crate) fn atomizable(asm: &Assembler, section: SectionId) -> bool {
    let Some(info) = asm.macho.sections.get(&section) else {
        return true;
    };
    !matches!(
        info.ty,
        S_CSTRING_LITERALS
            | S_4BYTE_LITERALS
            | S_8BYTE_LITERALS
            | S_16BYTE_LITERALS
            | S_LITERAL_POINTERS
            | S_NON_LAZY_SYMBOL_POINTERS
            | S_LAZY_SYMBOL_POINTERS
            | S_MOD_INIT_FUNC_POINTERS
            | S_MOD_TERM_FUNC_POINTERS
            | S_INTERPOSING
            | S_THREAD_LOCAL_VARIABLE_POINTERS
    )
}

/// Whether a PC-relative reference from fragment `frag` of `section` to
/// `target`, defined in that same section, still has to reach the linker.
///
/// Within one atom the answer is no: the two move together whatever the
/// linker does. Across atoms it is yes, since either may be dropped or moved
/// on its own — and unlike ELF, that has nothing to do with the symbol's
/// binding. That is all there is to it on x86-64. On arm64 llvm-mc resolves
/// a reference to an assembler-local label anywhere in the section, and one
/// to any label unless the file has `.subsections_via_symbols`, which is
/// what tells the linker it may really take the atoms apart.
pub(crate) fn defers_to_linker(
    asm: &Assembler,
    target: SymbolId,
    section: SectionId,
    frag: u32,
) -> bool {
    let Some(cpu) = Cpu::for_arch(asm.target()) else {
        return false;
    };
    // On a machine without reliable differences, llvm-mc takes any label to
    // be in the atom of whatever refers to it from the same section, unless
    // the file says its atoms are real with `.subsections_via_symbols`; and
    // an assembler-local label to be in it either way.
    if !cpu.reliable_symbol_difference()
        && (!asm.macho.subsections_via_symbols
            || is_temporary(asm.interner.get(asm.symbols.get(target).name)))
    {
        return false;
    }
    asm.macho.atoms.of(asm, target) != asm.macho.atoms.at(section, frag)
}

/// The section a defined symbol belongs to, whether it is a label there or
/// an alias of one.
fn section_of(asm: &Assembler, id: SymbolId) -> Option<SectionId> {
    match asm.symbols.get(id).value {
        SymbolValue::Label { section, .. } => Some(section),
        _ => asm.symbol_target_section(id).map(|(s, _)| s),
    }
}

/// Whether a relocation against `id` has to name the symbol rather than the
/// section it is in (`MachObjectWriter::doesSymbolRequireExternRelocation`).
///
/// An undefined symbol has no section to name. A weak definition has one, but
/// the linker may keep another file's definition instead, so the field cannot
/// be worked out here either.
fn requires_extern(asm: &Assembler, id: SymbolId) -> bool {
    let sym = asm.symbols.get(id);
    !sym.is_defined() || (asm.macho.desc(id) | weak_bits(sym.binding, true)) & N_WEAK_DEF != 0
}

/// The address a 32-bit object's relocation has to be worked out against,
/// where a scattered record is the only one that can say so.
///
/// Two references need one. A difference always does: the machine has one
/// relocation type and no addend field, so the only way to say `A - B` is to
/// record both addresses, in a `SECTDIFF` and the `PAIR` behind it. And so
/// does a reference some way into a symbol whose section the record would
/// otherwise name, since the linker cannot tell from a section which of the
/// symbols in it the field was measured from.
///
/// Everything else the ordinary record covers, and so does a reference the
/// scattered one cannot: its address is 24 bits wide, and llvm-mc writes the
/// ordinary record rather than lose the rest.
fn scattered(asm: &Assembler, cpu: Cpu, r: &Relocation, places: &Places) -> Option<i64> {
    if !cpu.generic_relocs() {
        return None;
    }
    let target = r.symbol?;
    if r.desc.subtrahend.is_some() {
        return Some(places.symbol(asm, target));
    }
    if r.addend == 0 || r.desc.class == RelocClass::ThreadVariable {
        return None;
    }
    // A debugging section holds values a debugger reads as they stand, and
    // llvm-mc names the section there rather than anything in it; see
    // [`debugging`].
    if debugging(asm, r.section) {
        return None;
    }
    // A symbol the record would have named anyway needs nothing more, and a
    // field more than 24 bits into its section is further than a scattered
    // record can say; llvm-mc writes the ordinary one rather than lose the
    // rest of the address.
    if requires_extern(asm, target) || r.offset > 0x00ff_ffff {
        return None;
    }
    // An absolute symbol is in no section, so there is nothing for the
    // linker to move the field by.
    section_of(asm, target)
        .is_some()
        .then(|| places.symbol(asm, target))
}

/// Whether a difference of two symbols in one section is a number the
/// assembler can work out, rather than a pair of relocations.
///
/// Only within an atom, on every machine; a difference that was a fixed
/// distance where the source wrote it was folded then, before there were
/// atoms (see `Assembler::macho_fixed_difference`).
pub(crate) fn folds_difference(asm: &Assembler, plus: SymbolId, minus: SymbolId) -> bool {
    asm.macho.atoms.of(asm, plus) == asm.macho.atoms.of(asm, minus)
}

/// The relocation type a fixup's description maps to, or `None` where the
/// machine has none — which is how `adr x0, sym` and a conditional branch to
/// another atom are refused, as llvm-mc refuses them.
pub(crate) fn reloc_type(cpu: Cpu, r: &Relocation) -> Option<u8> {
    let d = &r.desc;
    match cpu {
        Cpu::X86_64 => Some(match d.class {
            RelocClass::Branch if d.size == 4 => x86_64_reloc::BRANCH,
            // In data, `@GOTPCREL` is the slot relative to the field, with
            // the source supplying any bias itself.
            RelocClass::Got if d.size == 4 || (d.size == 8 && !d.pcrel) => x86_64_reloc::GOT,
            // Only a load of the slot itself can become a `leaq` of the symbol.
            RelocClass::GotLoad if d.pcrel && d.size == 4 && r.addend == 0 => {
                x86_64_reloc::GOT_LOAD
            }
            RelocClass::GotLoad if d.pcrel && d.size == 4 => x86_64_reloc::GOT,
            // llvm-mc picks the `SIGNED_n` variant by what the field holds,
            // which is the addend less the bytes after the field, rather than
            // by those bytes alone: `leaq _x-4(%rip)` is a `SIGNED_4` as well.
            RelocClass::Plain if d.pcrel && d.size == 4 => match r.addend + field_bias(cpu, r) {
                -1 => x86_64_reloc::SIGNED_1,
                -2 => x86_64_reloc::SIGNED_2,
                -4 => x86_64_reloc::SIGNED_4,
                _ => x86_64_reloc::SIGNED,
            },
            // A thread-local variable is reached through its descriptor,
            // which the loader fills in: a RIP-relative load of the
            // descriptor's address and nothing else.
            RelocClass::ThreadVariable if d.pcrel && d.size == 4 => x86_64_reloc::TLV,
            RelocClass::Plain if !d.pcrel => x86_64_reloc::UNSIGNED,
            // A sign-extended field can hold a difference, which the linker
            // checks, but not an address, which it could not.
            RelocClass::SignExtended if d.subtrahend.is_some() => x86_64_reloc::UNSIGNED,
            _ => return None,
        }),
        // A 32-bit object has one relocation for everything the field can
        // hold, and a thread-local one for the descriptor a `@TLVP` names.
        Cpu::I386 => Some(match d.class {
            RelocClass::Branch | RelocClass::Plain => generic_reloc::VANILLA,
            RelocClass::ThreadVariable if d.size == 4 => generic_reloc::TLV,
            RelocClass::SignExtended if d.subtrahend.is_some() => generic_reloc::VANILLA,
            _ => return None,
        }),
        Cpu::Arm64 => Some(match d.class {
            RelocClass::Branch if d.size == 4 => arm64_reloc::BRANCH26,
            RelocClass::Page => arm64_reloc::PAGE21,
            RelocClass::PageOff => arm64_reloc::PAGEOFF12,
            RelocClass::GotPage => arm64_reloc::GOT_LOAD_PAGE21,
            RelocClass::GotPageOff => arm64_reloc::GOT_LOAD_PAGEOFF12,
            RelocClass::Got if matches!(d.size, 4 | 8) => arm64_reloc::POINTER_TO_GOT,
            RelocClass::ThreadVariablePage => arm64_reloc::TLVP_LOAD_PAGE21,
            RelocClass::ThreadVariablePageOff => arm64_reloc::TLVP_LOAD_PAGEOFF12,
            RelocClass::Plain if !d.pcrel => arm64_reloc::UNSIGNED,
            _ => return None,
        }),
    }
}

/// Whether a relocation of type `ty` is PC-relative, which its entry says
/// whatever the fixup it came from was: an `adrp` is a page count to rsasm,
/// not a distance, but its relocation is still measured from the instruction.
fn entry_pcrel(cpu: Cpu, ty: u8, r: &Relocation) -> bool {
    match cpu {
        Cpu::X86_64 => !matches!(ty, x86_64_reloc::UNSIGNED | x86_64_reloc::SUBTRACTOR),
        // A `@TLVP` naming the descriptor itself covers no distance. The
        // same operator written as a difference from the PIC base does, and
        // the writer sets that entry's bit where it makes the entry.
        Cpu::I386 if ty == generic_reloc::TLV => false,
        Cpu::I386 => r.desc.pcrel,
        Cpu::Arm64 => match ty {
            arm64_reloc::BRANCH26
            | arm64_reloc::PAGE21
            | arm64_reloc::GOT_LOAD_PAGE21
            | arm64_reloc::TLVP_LOAD_PAGE21 => true,
            // As `sym@GOT - .`.
            arm64_reloc::POINTER_TO_GOT => r.desc.pcrel,
            _ => false,
        },
    }
}

/// Where a relocation of type `ty` keeps what the field would otherwise hold.
#[derive(Copy, Clone, PartialEq, Eq)]
enum AddendPlace {
    /// In the field, as a plain integer.
    Field,
    /// In an `ARM64_RELOC_ADDEND` entry ahead of it, the field holding zero:
    /// the instruction relocations, whose value is scattered through a word
    /// that has no room for more.
    Entry,
    /// Nowhere: an arm64 GOT relocation names the slot, which has no
    /// offset, and llvm-mc drops one written in data.
    None,
}

fn addend_place(cpu: Cpu, ty: u8) -> AddendPlace {
    match (cpu, ty) {
        (Cpu::X86_64 | Cpu::I386, _) => AddendPlace::Field,
        (Cpu::Arm64, arm64_reloc::BRANCH26 | arm64_reloc::PAGE21 | arm64_reloc::PAGEOFF12) => {
            AddendPlace::Entry
        }
        (Cpu::Arm64, arm64_reloc::UNSIGNED) => AddendPlace::Field,
        (Cpu::Arm64, _) => AddendPlace::None,
    }
}

/// What a PC-relative relocation's field holds beyond the addend.
///
/// Mach-O's PC-relative relocations are measured from the end of the
/// *field*. x86 measures a displacement from the end of the instruction, so
/// the field is short by the bytes of the instruction that follow it, and
/// `X86_64_RELOC_SIGNED_1/2/4` exist to tell the linker so, though llvm-mc
/// picks them by the result; a DWARF `DW_EH_PE_pcrel` field, measured from
/// its own start, is over by its own width in the same way. arm64's are all
/// measured from the instruction word, which is the field, so nothing is
/// left over there.
fn field_bias(cpu: Cpu, r: &Relocation) -> i64 {
    match cpu {
        Cpu::X86_64 | Cpu::I386 if r.desc.pcrel => -(r.desc.trailing as i64),
        _ => 0,
    }
}

// ---- section names ----------------------------------------------------------

/// A section a shorthand directive such as `.cstring` switches to.
#[derive(Copy, Clone, Debug)]
pub(crate) struct Shorthand {
    pub(crate) segment: &'static str,
    pub(crate) section: &'static str,
    pub(crate) ty: u32,
    pub(crate) attrs: u32,
    /// The alignment the directive gives the section, in bytes.
    pub(crate) align: u64,
    pub reserved2: u32,
}

/// The section a shorthand directive names, as Darwin's assembler defines
/// them: the pair, its type and attributes, and for the literal and pointer
/// sections an alignment of their element size.
pub(crate) fn shorthand(name: &str) -> Option<Shorthand> {
    let (segment, section, ty, attrs, align, reserved2) = match name {
        ".text" => (
            "__TEXT",
            "__text",
            S_REGULAR,
            S_ATTR_PURE_INSTRUCTIONS,
            1,
            0,
        ),
        ".data" => ("__DATA", "__data", S_REGULAR, 0, 1, 0),
        ".bss" => ("__DATA", "__bss", S_ZEROFILL, 0, 1, 0),
        ".const" => ("__TEXT", "__const", S_REGULAR, 0, 1, 0),
        ".static_const" => ("__TEXT", "__static_const", S_REGULAR, 0, 1, 0),
        ".cstring" => ("__TEXT", "__cstring", S_CSTRING_LITERALS, 0, 1, 0),
        ".literal4" => ("__TEXT", "__literal4", S_4BYTE_LITERALS, 0, 4, 0),
        ".literal8" => ("__TEXT", "__literal8", S_8BYTE_LITERALS, 0, 8, 0),
        ".literal16" => ("__TEXT", "__literal16", S_16BYTE_LITERALS, 0, 16, 0),
        ".constructor" => ("__TEXT", "__constructor", S_REGULAR, 0, 1, 0),
        ".destructor" => ("__TEXT", "__destructor", S_REGULAR, 0, 1, 0),
        ".const_data" => ("__DATA", "__const", S_REGULAR, 0, 1, 0),
        ".static_data" => ("__DATA", "__static_data", S_REGULAR, 0, 1, 0),
        ".mod_init_func" => (
            "__DATA",
            "__mod_init_func",
            S_MOD_INIT_FUNC_POINTERS,
            0,
            4,
            0,
        ),
        ".mod_term_func" => (
            "__DATA",
            "__mod_term_func",
            S_MOD_TERM_FUNC_POINTERS,
            0,
            4,
            0,
        ),
        ".non_lazy_symbol_pointer" => (
            "__DATA",
            "__nl_symbol_ptr",
            S_NON_LAZY_SYMBOL_POINTERS,
            0,
            4,
            0,
        ),
        ".lazy_symbol_pointer" => ("__DATA", "__la_symbol_ptr", S_LAZY_SYMBOL_POINTERS, 0, 4, 0),
        ".tdata" => ("__DATA", "__thread_data", S_THREAD_LOCAL_REGULAR, 0, 1, 0),
        ".tlv" => ("__DATA", "__thread_vars", S_THREAD_LOCAL_VARIABLES, 0, 1, 0),
        ".thread_init_func" => (
            "__DATA",
            "__thread_init",
            S_THREAD_LOCAL_INIT_FUNCTION_POINTERS,
            0,
            1,
            0,
        ),
        _ => return None,
    };
    Some(Shorthand {
        segment,
        section,
        ty,
        attrs,
        align,
        reserved2,
    })
}

/// The section type a `.section` directive's third argument names.
pub(crate) fn section_type(name: &str) -> Option<u32> {
    Some(match name {
        "regular" => S_REGULAR,
        "cstring_literals" => S_CSTRING_LITERALS,
        "4byte_literals" => S_4BYTE_LITERALS,
        "8byte_literals" => S_8BYTE_LITERALS,
        "16byte_literals" => S_16BYTE_LITERALS,
        "literal_pointers" => S_LITERAL_POINTERS,
        "non_lazy_symbol_pointers" => S_NON_LAZY_SYMBOL_POINTERS,
        "lazy_symbol_pointers" => S_LAZY_SYMBOL_POINTERS,
        "symbol_stubs" => S_SYMBOL_STUBS,
        "mod_init_funcs" => S_MOD_INIT_FUNC_POINTERS,
        "mod_term_funcs" => S_MOD_TERM_FUNC_POINTERS,
        "coalesced" => S_COALESCED,
        "zerofill" => S_ZEROFILL,
        "gb_zerofill" => S_GB_ZEROFILL,
        "interposing" => S_INTERPOSING,
        "thread_local_regular" => S_THREAD_LOCAL_REGULAR,
        "thread_local_zerofill" => S_THREAD_LOCAL_ZEROFILL,
        "thread_local_variables" => S_THREAD_LOCAL_VARIABLES,
        "thread_local_variable_pointers" => S_THREAD_LOCAL_VARIABLE_POINTERS,
        "thread_local_init_function_pointers" => S_THREAD_LOCAL_INIT_FUNCTION_POINTERS,
        _ => return None,
    })
}

/// The `S_ATTR_*` bit a `.section` attribute name asks for.
pub(crate) fn section_attribute(name: &str) -> Option<u32> {
    Some(match name {
        "none" => 0,
        "pure_instructions" => S_ATTR_PURE_INSTRUCTIONS,
        "no_toc" => S_ATTR_NO_TOC,
        "strip_static_syms" => S_ATTR_STRIP_STATIC_SYMS,
        "no_dead_strip" => S_ATTR_NO_DEAD_STRIP,
        "live_support" => S_ATTR_LIVE_SUPPORT,
        "self_modifying_code" => S_ATTR_SELF_MODIFYING_CODE,
        "debug" => S_ATTR_DEBUG,
        _ => return None,
    })
}

/// The type and attributes of a section llvm-mc knows before it reads any
/// source, which it keeps whatever a `.section` directive naming it says.
///
/// The target decides one of them: `MCObjectFileInfo` names
/// `__LD,__compact_unwind` only where the object has a compact unwind table,
/// so without one the pair is an ordinary section.
pub(crate) fn precreated(
    deployment: &Deployment,
    segment: &str,
    section: &str,
) -> Option<(u32, u32)> {
    Some(match (segment, section) {
        ("__TEXT", "__text") => (S_REGULAR, S_ATTR_PURE_INSTRUCTIONS),
        ("__TEXT", "__cstring") => (S_CSTRING_LITERALS, 0),
        ("__TEXT", "__literal4") => (S_4BYTE_LITERALS, 0),
        ("__TEXT", "__literal8") => (S_8BYTE_LITERALS, 0),
        ("__TEXT", "__literal16") => (S_16BYTE_LITERALS, 0),
        ("__TEXT", "__const") => (S_REGULAR, 0),
        ("__DATA", "__data") => (S_REGULAR, 0),
        ("__DATA", "__const") => (S_REGULAR, 0),
        ("__DATA", "__bss") => (S_ZEROFILL, 0),
        ("__DATA", "__common") => (S_ZEROFILL, 0),
        ("__DATA", "__mod_init_func") => (S_MOD_INIT_FUNC_POINTERS, 0),
        ("__DATA", "__mod_term_func") => (S_MOD_TERM_FUNC_POINTERS, 0),
        ("__DATA", "__la_symbol_ptr") => (S_LAZY_SYMBOL_POINTERS, 0),
        ("__DATA", "__nl_symbol_ptr") => (S_NON_LAZY_SYMBOL_POINTERS, 0),
        ("__DATA", "__thread_vars") => (S_THREAD_LOCAL_VARIABLES, 0),
        ("__DATA", "__thread_bss") => (S_THREAD_LOCAL_ZEROFILL, 0),
        ("__DATA", "__thread_data") => (S_THREAD_LOCAL_REGULAR, 0),
        ("__DATA", "__thread_init") => (S_THREAD_LOCAL_INIT_FUNCTION_POINTERS, 0),
        // The debugging sections, which a linker copies without looking
        // inside and a `strip` drops; the segment alone does not say so, and
        // `__DWARF,__foo` is an ordinary section.
        (
            "__DWARF",
            "__debug_line" | "__debug_line_str" | "__debug_info" | "__debug_abbrev" | "__debug_str"
            | "__debug_aranges" | "__debug_ranges" | "__debug_rnglists" | "__debug_frame",
        ) => (S_REGULAR, S_ATTR_DEBUG),
        // The compact unwind table is read by the linker alone, and
        // dropped once it has read it.
        ("__LD", "__compact_unwind") if deployment.compact_unwind.is_some() => {
            (S_REGULAR, S_ATTR_DEBUG)
        }
        // The frame table is one item per function, which the linker keeps
        // or drops with the function it describes: coalesced, live-support,
        // and with no static symbols of its own to keep.
        ("__TEXT", "__eh_frame") => (
            S_COALESCED,
            S_ATTR_NO_TOC | S_ATTR_STRIP_STATIC_SYMS | S_ATTR_LIVE_SUPPORT,
        ),
        _ => return None,
    })
}

/// Whether llvm-mc gives the section a start symbol of its own, which takes
/// the place of the `ltmpN` label an arm64 object would otherwise carry
/// there (`MCMachOStreamer::changeSection` leaves a section that has one
/// alone).
///
/// `MCObjectFileInfo` names one for each debugging section it creates, and
/// the symbol is dropped from the table again because nothing relocates
/// against it; `__debug_aranges` is the one it creates without.
fn has_start_symbol(deployment: &Deployment, segment: &str, section: &str) -> bool {
    segment == "__DWARF"
        && section != "__debug_aranges"
        && precreated(deployment, segment, section).is_some()
}

/// Whether a linker-visible label starts an atom strictly after `from` and
/// at or before `to` (in either order), so that the two positions are in
/// different atoms. Positions are `(section, fragment, definition order)`;
/// see [`Atoms`].
pub(crate) fn atom_starts_between(
    interner: &crate::intern::Interner,
    symbols: &crate::symbol::SymbolTable,
    from: (SectionId, u32, u32),
    to: (SectionId, u32, u32),
) -> bool {
    let (a, b) = ((from.1, from.2), (to.1, to.2));
    let (lo, hi) = (a.min(b), a.max(b));
    symbols.iter().any(|(_, sym)| match sym.value {
        SymbolValue::Label { section, frag } => {
            let at = (frag, sym.def_order);
            section == from.0 && at > lo && at <= hi && !is_temporary(interner.get(sym.name))
        }
        _ => false,
    })
}

/// Splits a section's rsasm name back into its Mach-O pair. In Mach-O output
/// every section is named `SEGMENT,SECTION`, which is what the directives
/// store, so this only has to fail on a name from somewhere else.
pub(crate) fn split_name(name: &str) -> Option<(&str, &str)> {
    name.split_once(',')
}

// ---- writing ----------------------------------------------------------------

/// A section as the object will hold it.
struct Sec {
    id: SectionId,
    segment: String,
    section: String,
    flags: u32,
    reserved2: u32,
    /// Alignment as a power of two, which is how Mach-O stores it.
    align: u32,
    addr: u64,
    size: u64,
    zerofill: bool,
    bytes: Vec<u8>,
    relocs: Vec<Entry>,
}

/// One relocation record: a `relocation_info`, or the
/// `scattered_relocation_info` a 32-bit object writes where the ordinary
/// record cannot say what the field holds; see [`scattered`].
struct Entry {
    address: u32,
    symbolnum: u32,
    pcrel: bool,
    length: u8,
    external: bool,
    ty: u8,
    /// The address the field was worked out against, which only a scattered
    /// record carries. It takes the place of the symbol index, and the linker
    /// moves the field by however far that address moves; where the record
    /// has one, the other machines' `r_extern` and `r_symbolnum` are not
    /// there to be written.
    value: Option<u32>,
}

impl Entry {
    fn words(&self) -> (u32, u32) {
        match self.value {
            // A scattered record packs the address, type and widths into the
            // first word, leaving the second for the value; the top bit is
            // what tells the two records apart.
            Some(value) => (
                (self.address & 0x00ff_ffff)
                    | (u32::from(self.ty) << 24)
                    | (u32::from(self.length) << 28)
                    | ((self.pcrel as u32) << 30)
                    | R_SCATTERED,
                value,
            ),
            None => (
                self.address,
                (self.symbolnum & 0x00ff_ffff)
                    | ((self.pcrel as u32) << 24)
                    | (u32::from(self.length) << 25)
                    | ((self.external as u32) << 27)
                    | (u32::from(self.ty) << 28),
            ),
        }
    }
}

/// A symbol as the object will hold it.
struct OutSym {
    name: String,
    n_type: u8,
    n_sect: u8,
    n_desc: u16,
    n_value: u64,
}

/// What a relocation entry names, once the atom rules have been applied.
#[derive(Copy, Clone)]
enum Named {
    /// A symbol of the assembler's.
    Symbol(SymbolId),
    /// A section, by its own index: a local relocation, with the target's
    /// address left in the field.
    Section(SectionId),
    /// The `ltmpN` label at the start of a section; see
    /// [`Cpu::labels_sections`].
    SectionLabel(SectionId),
}

/// The indirect symbol table and where each pointer section's own part of it
/// begins; see [`State::indirect_bound`].
struct Indirect {
    /// One entry per `.indirect_symbol`, in the order they were written.
    entries: Vec<u32>,
    /// The index of a section's first entry, which is its `reserved1`.
    bases: HashMap<SectionId, u32>,
}

/// Leaves `field` in the bytes `r` covers, where the section has them: a
/// zero-filled section holds none, and neither does a reference past the end
/// of one, which the layout pass has already reported.
fn write_field(sec: &mut Sec, r: &Relocation, field: i64) {
    let (off, size) = (r.offset as usize, r.desc.size as usize);
    if off + size <= sec.bytes.len() {
        crate::arch::Endian::Little.write(&mut sec.bytes[off..off + size], field as u64);
    }
}

/// Section addresses and indices, which everything past layout refers to.
struct Places {
    index: HashMap<SectionId, usize>,
    addr: HashMap<SectionId, u64>,
}

impl Places {
    /// A symbol's address in the object, which unlike its offset counts the
    /// sections before its own.
    fn symbol(&self, asm: &Assembler, id: SymbolId) -> i64 {
        let Some(offset) = asm.symbol_addr(id) else {
            return 0;
        };
        offset
            + section_of(asm, id)
                .and_then(|s| self.addr.get(&s))
                .copied()
                .unwrap_or(0) as i64
    }

    /// What a named symbol or section adds when the linker applies a
    /// relocation against it, as things stand in this object. A local
    /// relocation adds nothing: the linker moves the field by as much as the
    /// section moves instead.
    fn named(&self, asm: &Assembler, n: Named) -> i64 {
        match n {
            Named::Symbol(id) => self.symbol(asm, id),
            Named::Section(_) => 0,
            Named::SectionLabel(s) => self.addr.get(&s).copied().unwrap_or(0) as i64,
        }
    }
}

pub fn build(asm: &Assembler) -> Result<Vec<u8>, OutputError> {
    let cpu = Cpu::for_arch(asm.target()).ok_or_else(|| {
        OutputError::Unsupported(format!(
            "Mach-O output has no machine for `{}`; only x86-64, i386 and arm64 have one",
            asm.target().name()
        ))
    })?;

    if !asm.options.relocatable {
        return Err(OutputError::Unsupported(
            "`--base` lays out a flat binary; a Mach-O object is placed by the linker".into(),
        ));
    }
    if asm.options.dialect == crate::lexer::Dialect::Nasm {
        return Err(OutputError::Unsupported(
            "NASM source is assembled to ELF objects and flat binaries; Mach-O output \
             reads GNU-style source"
                .into(),
        ));
    }

    let mut secs = collect_sections(asm)?;
    assign_addresses(&mut secs);
    let places = Places {
        index: secs.iter().enumerate().map(|(i, s)| (s.id, i)).collect(),
        addr: secs.iter().map(|s| (s.id, s.addr)).collect(),
    };

    // What each relocation names. An assembler-local label can end up in the
    // symbol table because a relocation has to name it, and whether one does
    // depends on the relocations before it, so this runs over all of them in
    // order before the table is built.
    let mut visible: HashSet<SymbolId> = HashSet::new();
    let named: Vec<(Named, Option<Named>)> = asm
        .relocs
        .iter()
        .map(|r| {
            let a = name_target(asm, cpu, r.symbol, r, &mut visible, true);
            let b = r
                .desc
                .subtrahend
                .map(|s| name_target(asm, cpu, Some(s), r, &mut visible, false));
            (a, b)
        })
        .collect();

    let table = collect_symbols(asm, cpu, &secs, &places, &visible);
    let (syms, index, counts) = (&table.syms, &table.index, table.counts);
    let number = |n: Named| -> Result<(u32, bool), OutputError> {
        Ok(match n {
            Named::Symbol(id) => match index.get(&id) {
                Some(&i) => (i, true),
                None => {
                    return Err(OutputError::Unsupported(format!(
                        "`{}` is relocated against, but has no symbol table entry",
                        asm.display_name(id)
                    )));
                }
            },
            Named::Section(s) => (places.index[&s] as u32 + 1, false),
            Named::SectionLabel(s) => (table.labels[&s], true),
        })
    };

    for (r, &(a, b)) in asm.relocs.iter().zip(&named) {
        let Some(&si) = places.index.get(&r.section) else {
            continue;
        };
        // A symbol an FDE points at is a value this writer can work out,
        // since every section already has an address; llvm-mc writes the
        // number and leaves no relocation behind.
        if r.desc.class == RelocClass::FrameSymbol {
            let here = places.addr[&r.section] as i64 + r.offset as i64;
            let target = r.symbol.map_or(0, |t| places.symbol(asm, t)) + r.addend;
            let value = if r.desc.pcrel { target - here } else { target };
            write_field(&mut secs[si], r, value);
            continue;
        }
        let ty = reloc_type(cpu, r).ok_or_else(|| {
            OutputError::Unsupported("a reference here has no Mach-O relocation".into())
        })?;
        let length = match r.desc.size {
            1 => 0,
            2 => 1,
            4 => 2,
            _ => 3,
        };
        let address = r.offset as u32;
        let here = places.addr[&r.section] as i64 + r.offset as i64;
        let target = r.symbol.map_or(0, |t| places.symbol(asm, t)) + r.addend;
        // A 32-bit `@TLVP` names the descriptor and nothing else. Written
        // as a difference from the PIC base, which is how
        // position-independent code reaches it, the distance from that base
        // is left in the field and the base is not named at all.
        if cpu.generic_relocs()
            && ty == generic_reloc::TLV
            && let Some(sub) = r.desc.subtrahend
        {
            let field = here + r.desc.size as i64 - places.symbol(asm, sub) + r.addend;
            write_field(&mut secs[si], r, field);
            let (symbolnum, external) = number(a)?;
            secs[si].relocs.push(Entry {
                address,
                symbolnum,
                pcrel: true,
                length,
                external,
                ty,
                value: None,
            });
            continue;
        }

        // A 32-bit object records an address where the ordinary record
        // could not say what the field holds; see [`scattered`].
        if let Some(value) = scattered(asm, cpu, r, &places) {
            let mut entries = Vec::new();
            let mut field = target;
            let ty = match r.desc.subtrahend {
                Some(sub) => {
                    if address > 0x00ff_ffff {
                        return Err(OutputError::Unsupported(format!(
                            "a difference of symbols {address} bytes into its section cannot be \
                             relocated; a scattered relocation's address is 24 bits wide"
                        )));
                    }
                    let minus = places.symbol(asm, sub);
                    field -= minus;
                    // The `PAIR` holding the subtracted address is written
                    // first, so that reversing the section's records below
                    // leaves it behind the record it belongs to.
                    entries.push(Entry {
                        address: 0,
                        symbolnum: 0,
                        pcrel: r.desc.pcrel,
                        length,
                        external: false,
                        ty: generic_reloc::PAIR,
                        value: Some(minus as u32),
                    });
                    // The linker reads the two difference relocations alike;
                    // llvm-mc tells them apart as Darwin's own assembler
                    // did, by whether the symbol added is a global one.
                    let global =
                        asm.symbols.get(r.symbol.expect("named")).binding != Binding::Local;
                    if global {
                        generic_reloc::SECTDIFF
                    } else {
                        generic_reloc::LOCAL_SECTDIFF
                    }
                }
                None => ty,
            };
            if r.desc.pcrel {
                field += field_bias(cpu, r) - (here + r.desc.size as i64);
            }
            entries.push(Entry {
                address,
                symbolnum: 0,
                pcrel: r.desc.pcrel,
                length,
                external: false,
                ty,
                value: Some(value as u32),
            });
            write_field(&mut secs[si], r, field);
            secs[si].relocs.append(&mut entries);
            continue;
        }

        let (symbolnum, external) = number(a)?;

        // The field holds what the linker's arithmetic leaves out: the value
        // less what the named symbols will contribute at their final
        // addresses.
        let mut entries = Vec::new();
        let mut field;
        match b {
            Some(b) => {
                // `A - B`: the `UNSIGNED` naming A and the `SUBTRACTOR` naming
                // B, both over the same field; reversed below, like the rest.
                let sub = places.symbol(asm, r.desc.subtrahend.expect("paired"));
                field = (target - sub) - places.named(asm, a) + places.named(asm, b);
                let (bnum, bext) = number(b)?;
                let subtractor = match cpu {
                    Cpu::X86_64 => x86_64_reloc::SUBTRACTOR,
                    Cpu::Arm64 => arm64_reloc::SUBTRACTOR,
                    // Handled above, by a scattered record.
                    Cpu::I386 => unreachable!("a 32-bit difference is scattered"),
                };
                entries.push(Entry {
                    address,
                    symbolnum,
                    pcrel: false,
                    length,
                    external,
                    ty,
                    value: None,
                });
                entries.push(Entry {
                    address,
                    symbolnum: bnum,
                    pcrel: false,
                    length,
                    external: bext,
                    ty: subtractor,
                    value: None,
                });
            }
            None => {
                field = target - places.named(asm, a);
                if r.desc.pcrel {
                    field += field_bias(cpu, r);
                    // A local PC-relative field keeps the distance as the
                    // assembler measured it, from the end of the field. So
                    // does a generic relocation's whatever it names: the
                    // linker adds the symbol's address to what it finds,
                    // where x86-64's and arm64's measure the distance
                    // themselves and are given the addend alone.
                    if !external || cpu.generic_relocs() {
                        field -= here + r.desc.size as i64;
                    }
                }
                entries.push(Entry {
                    address,
                    symbolnum,
                    pcrel: entry_pcrel(cpu, ty, r),
                    length,
                    external,
                    ty,
                    value: None,
                });
                match addend_place(cpu, ty) {
                    AddendPlace::Field => {}
                    AddendPlace::Entry if field != 0 => {
                        if !(-0x0080_0000..0x0080_0000).contains(&field) {
                            return Err(OutputError::Unsupported(format!(
                                "an addend of {field} does not fit in an `ARM64_RELOC_ADDEND`"
                            )));
                        }
                        // The addend takes the entry's symbol number, as
                        // 24-bit two's complement.
                        entries.push(Entry {
                            address,
                            symbolnum: field as u32 & 0x00ff_ffff,
                            pcrel: false,
                            length,
                            external: false,
                            ty: arm64_reloc::ADDEND,
                            value: None,
                        });
                        field = 0;
                    }
                    AddendPlace::Entry => {}
                    // llvm-mc keeps the constant of `sym@GOT`, less the offset
                    // of `.` in its section for `sym@GOT - .`.
                    AddendPlace::None if ty == arm64_reloc::POINTER_TO_GOT => {
                        field = r.addend - if r.desc.pcrel { r.offset as i64 } else { 0 };
                    }
                    AddendPlace::None if field != 0 => {
                        return Err(OutputError::Unsupported(format!(
                            "a GOT reference to `{}` lies {field} bytes into the symbol a \
                             Mach-O relocation can name, and a GOT slot has no offset",
                            r.symbol.map_or_else(String::new, |s| asm.display_name(s))
                        )));
                    }
                    AddendPlace::None => field = 0,
                }
            }
        }
        let in_field =
            addend_place(cpu, ty) == AddendPlace::Field || ty == arm64_reloc::POINTER_TO_GOT;
        if in_field {
            write_field(&mut secs[si], r, field);
        }
        secs[si].relocs.append(&mut entries);
    }
    // llvm-mc writes each section's relocations last first, which also puts
    // the `SUBTRACTOR` of each pair and an `ARM64_RELOC_ADDEND` ahead of the
    // entry they modify.
    for s in &mut secs {
        s.relocs.reverse();
    }

    // Each region as its address and length.
    let regions: Vec<(u32, u16, u16)> = asm
        .macho
        .data_regions
        .iter()
        .filter_map(|d| {
            let start = places.symbol(asm, d.start);
            let end = places.symbol(asm, d.end?);
            Some((start as u32, (end - start) as u16, d.kind))
        })
        .collect();

    // The indirect symbol table, in the order the directives were written: a
    // symbol's index, or, for a pointer the linker needs no symbol to fill
    // in, the flags that say so.
    let entries: Vec<u32> = asm
        .macho
        .indirect_bound
        .iter()
        .map(|&(section, id)| {
            let non_lazy = asm
                .macho
                .sections
                .get(&section)
                .is_some_and(|i| i.ty == S_NON_LAZY_SYMBOL_POINTERS);
            let entry = index.get(&id).map(|&i| &syms[i as usize]);
            let local = entry.is_some_and(|s| s.n_type & N_EXT == 0);
            match entry {
                Some(s) if non_lazy && local => {
                    let abs = s.n_type & N_TYPE == N_ABS;
                    INDIRECT_SYMBOL_LOCAL | if abs { INDIRECT_SYMBOL_ABS } else { 0 }
                }
                _ => index.get(&id).copied().unwrap_or(0),
            }
        })
        .collect();
    let mut bases: HashMap<SectionId, u32> = HashMap::new();
    for (i, &(section, _)) in asm.macho.indirect_bound.iter().enumerate() {
        bases.entry(section).or_insert(i as u32);
    }
    let indirect = Indirect { entries, bases };

    Ok(write(asm, cpu, &secs, syms, counts, &regions, &indirect))
}

/// Every section of the object, in the order the source created them. Unlike
/// ELF, Mach-O keeps a section with nothing in it.
fn collect_sections(asm: &Assembler) -> Result<Vec<Sec>, OutputError> {
    let mut out = Vec::new();
    for s in &asm.sections {
        let name = asm.interner.get(s.name).to_string();
        let (segment, section) = split_name(&name).ok_or_else(|| {
            OutputError::Unsupported(format!(
                "`{name}` is not a Mach-O section; Mach-O sections are named `SEGMENT,SECTION`"
            ))
        })?;
        let info = asm.macho.sections.get(&s.id);
        let (ty, attrs) = match info {
            Some(i) => (i.ty, i.attrs),
            None => precreated(&asm.macho.deployment, segment, section).unwrap_or((S_REGULAR, 0)),
        };
        // llvm-mc marks a section that any instruction was assembled into.
        let attrs = attrs
            | if s.has_instructions {
                S_ATTR_SOME_INSTRUCTIONS
            } else {
                0
            };
        let zerofill = matches!(ty, S_ZEROFILL | S_GB_ZEROFILL | S_THREAD_LOCAL_ZEROFILL);
        out.push(Sec {
            id: s.id,
            segment: segment.to_string(),
            section: section.to_string(),
            flags: ty | attrs,
            reserved2: info.map_or(0, |i| i.reserved2),
            align: s.align.max(1).trailing_zeros(),
            addr: 0,
            size: s.size,
            zerofill,
            bytes: if zerofill {
                Vec::new()
            } else {
                asm.section_bytes(s.id)
            },
            relocs: Vec::new(),
        });
    }
    Ok(out)
}

/// Gives every section an address in the object's single address space:
/// those with contents first, in order, then the zero-filled ones, which the
/// segment's file image cannot have in between.
fn assign_addresses(secs: &mut [Sec]) {
    let mut addr = 0u64;
    for zerofill in [false, true] {
        for s in secs.iter_mut().filter(|s| s.zerofill == zerofill) {
            addr = addr.next_multiple_of(1u64 << s.align);
            s.addr = addr;
            addr += s.size;
        }
    }
}

/// Whether a section is one of the debugging sections, whose relocations
/// llvm-mc keeps local wherever it can: a debugger reads a debugging section
/// expecting the values in it to be filled in already, so
/// `MachObjectWriter::recordRelocation` names the section rather than the
/// atom a symbol belongs to.
fn debugging(asm: &Assembler, section: SectionId) -> bool {
    asm.macho
        .sections
        .get(&section)
        .is_some_and(|i| i.attrs & S_ATTR_DEBUG != 0)
}

/// What a relocation against `target` names.
///
/// A relocation in a debugging section names the section its target is in,
/// whatever the target is; see [`debugging`].
///
/// A linker-visible or undefined symbol names itself. An assembler-local
/// label names the atom it is in; with no atom, x86-64 names the section and
/// leaves the address in the field, and arm64 names the label llvm-mc puts at
/// the start of every section. A label in a literal section has to be named
/// itself where a position could not say which item it means, since the
/// linker takes such a section apart, and that puts it in the symbol table:
/// always on arm64, and on x86-64 where the field would hold more than the
/// label's address — llvm-mc's test is a non-zero addend, and once passed it
/// holds for every later relocation too, which is what `visible` carries.
/// Only the added symbol of a pair is subject to that test.
fn name_target(
    asm: &Assembler,
    cpu: Cpu,
    target: Option<SymbolId>,
    r: &Relocation,
    visible: &mut HashSet<SymbolId>,
    added: bool,
) -> Named {
    let Some(target) = target else {
        return Named::Section(r.section);
    };
    let sym = asm.symbols.get(target);
    // A generic relocation names the section wherever the field can be
    // worked out here, which leaves only an undefined symbol and a weak
    // definition to be named; see [`requires_extern`]. An atom means nothing
    // to it: a reference into one carries the address it was measured from
    // in a scattered record instead; see [`scattered`].
    if cpu.generic_relocs() {
        // A thread-local reference names the descriptor the loader fills in,
        // whose section says nothing about it.
        let thread_local = added && r.desc.class == RelocClass::ThreadVariable;
        if thread_local || requires_extern(asm, target) {
            return Named::Symbol(target);
        }
        return match section_of(asm, target) {
            Some(section) => Named::Section(section),
            None => Named::Symbol(target),
        };
    }
    if debugging(asm, r.section)
        && let SymbolValue::Label { section, .. } = sym.value
    {
        return Named::Section(section);
    }
    if !is_temporary(asm.interner.get(sym.name)) || !sym.is_defined() || visible.contains(&target) {
        return Named::Symbol(target);
    }
    let SymbolValue::Label { section, .. } = sym.value else {
        return Named::Symbol(target);
    };
    if let Some(atom) = asm.macho.atoms.of(asm, target) {
        return Named::Symbol(atom);
    }
    let literal = !atomizable(asm, section);
    match cpu {
        Cpu::Arm64 if literal => {
            visible.insert(target);
            Named::Symbol(target)
        }
        Cpu::Arm64 => Named::SectionLabel(section),
        Cpu::X86_64 if literal && added && r.addend + field_bias(cpu, r) != 0 => {
            visible.insert(target);
            Named::Symbol(target)
        }
        // A generic relocation answered above.
        Cpu::X86_64 | Cpu::I386 => Named::Section(section),
    }
}

/// The symbol table: locals first, then defined externals, then undefined
/// symbols, each of the last two sorted by name as Mach-O requires.
fn collect_symbols(
    asm: &Assembler,
    cpu: Cpu,
    secs: &[Sec],
    places: &Places,
    visible: &HashSet<SymbolId>,
) -> Symtab {
    // `.indirect_symbol` is enough on its own to put a symbol in the table,
    // and where the only thing that named it is a lazy pointer or a stub, the
    // linker is asked to bind it lazily
    // (`MachObjectWriter::bindIndirectSymbols`, which binds the pointers that
    // are not lazy first, so a symbol in both kinds of section is not lazy).
    let mut indirect: HashMap<SymbolId, bool> = HashMap::new();
    for lazy_pass in [false, true] {
        for &(section, id) in &asm.macho.indirect_bound {
            let lazy = matches!(
                asm.macho.sections.get(&section).map(|i| i.ty),
                Some(S_LAZY_SYMBOL_POINTERS | S_SYMBOL_STUBS)
            );
            if lazy != lazy_pass || indirect.contains_key(&id) {
                continue;
            }
            let sym = asm.symbols.get(id);
            let named = sym.is_defined() || sym.used || sym.declared;
            indirect.insert(id, lazy && !named);
        }
    }
    let mut locals: Vec<(Local, OutSym)> = Vec::new();
    let mut externals: Vec<(SymbolId, OutSym)> = Vec::new();
    let mut undefined: Vec<(SymbolId, OutSym)> = Vec::new();

    // The label arm64 objects carry at the start of each section is a local
    // symbol like any other, created when the section was: it goes among the
    // others where that happened, as llvm-mc has it. The sections llvm-mc
    // makes with a start symbol of its own get none, and the labels are
    // numbered over those that do.
    let mut next = 0;
    let mut labels = secs
        .iter()
        .enumerate()
        .filter(|(_, s)| {
            cpu.labels_sections()
                && !has_start_symbol(&asm.macho.deployment, &s.segment, &s.section)
        })
        .map(|(i, s)| {
            let mark = asm.macho.section_marks.get(&s.id).copied().unwrap_or(0);
            let label = OutSym {
                name: format!("ltmp{next}"),
                n_type: N_SECT,
                n_sect: i as u8 + 1,
                n_desc: 0,
                n_value: s.addr,
            };
            next += 1;
            (mark, s.id, label)
        })
        .collect::<Vec<_>>()
        .into_iter()
        .peekable();

    for (id, sym) in asm.symbols.iter() {
        while let Some((_, section, label)) = labels.next_if(|l| l.0 <= id.0) {
            locals.push((Local::SectionLabel(section), label));
        }
        let name = asm.interner.get(sym.name).to_string();
        if sym.ty == crate::symbol::SymType::Section {
            continue;
        }
        if is_temporary(&name) && !visible.contains(&id) {
            continue;
        }
        if !sym.is_defined() && !sym.used && !indirect.contains_key(&id) {
            continue;
        }
        let desc = asm.macho.desc(id)
            | if indirect.get(&id) == Some(&true) {
                REFERENCE_FLAG_UNDEFINED_LAZY
            } else {
                0
            };
        let global = sym.binding != Binding::Local;
        let section_of = |section: SectionId| places.index.get(&section).map(|&i| i as u8 + 1);
        let out = match &sym.value {
            SymbolValue::Undefined => OutSym {
                name,
                n_type: N_UNDF | N_EXT,
                n_sect: 0,
                n_desc: desc | weak_bits(sym.binding, false),
                n_value: 0,
            },
            SymbolValue::Common { size, align } => OutSym {
                name,
                n_type: N_UNDF | N_EXT,
                n_sect: 0,
                // A common symbol's alignment lives in its description.
                n_desc: desc | (((*align).max(1).trailing_zeros() as u16) << 8),
                n_value: *size,
            },
            SymbolValue::Label { section, .. } => {
                let Some(n_sect) = section_of(*section) else {
                    continue;
                };
                OutSym {
                    name,
                    n_type: N_SECT | ext_bits(global, sym.visibility),
                    n_sect,
                    n_desc: desc | weak_bits(sym.binding, true),
                    n_value: places.symbol(asm, id) as u64,
                }
            }
            SymbolValue::Expr(e) => match asm.symbol_target_section(id) {
                Some((section, _)) => {
                    let Some(n_sect) = section_of(section) else {
                        continue;
                    };
                    // An alias of a position inside a label's code, rather
                    // than of the label, is an alternate entry into it. An
                    // alias of the label itself is the label as far as the
                    // symbol table is concerned, and carries none of the
                    // bits the directive would otherwise have given it.
                    let inside = asm.eval_ref(*e).is_ok_and(|v| v.addend != 0);
                    let alt = if inside {
                        N_ALT_ENTRY
                            | if asm.macho.set_constants.contains(&id) {
                                N_NO_DEAD_STRIP
                            } else {
                                0
                            }
                    } else {
                        0
                    };
                    OutSym {
                        name,
                        n_type: N_SECT | ext_bits(global, sym.visibility),
                        n_sect,
                        n_desc: desc | weak_bits(sym.binding, true) | alt,
                        n_value: places.symbol(asm, id) as u64,
                    }
                }
                None => OutSym {
                    name,
                    n_type: N_ABS | ext_bits(global, sym.visibility),
                    n_sect: 0,
                    // llvm-mc marks a constant given by `.set` or `.equ`, but
                    // not one given by `=`, as not to be dead-stripped.
                    n_desc: desc
                        | if asm.macho.set_constants.contains(&id) {
                            N_NO_DEAD_STRIP
                        } else {
                            0
                        },
                    n_value: asm.symbol_number(id).unwrap_or(0) as u64,
                },
            },
        };
        if out.n_type & N_EXT == 0 {
            locals.push((Local::Symbol(id), out));
        } else if out.n_type & N_TYPE == N_UNDF {
            undefined.push((id, out));
        } else {
            externals.push((id, out));
        }
    }
    for (_, section, label) in labels {
        locals.push((Local::SectionLabel(section), label));
    }

    externals.sort_by(|a, b| a.1.name.cmp(&b.1.name));
    undefined.sort_by(|a, b| a.1.name.cmp(&b.1.name));

    let mut table = Symtab {
        counts: (
            locals.len() as u32,
            externals.len() as u32,
            undefined.len() as u32,
        ),
        ..Symtab::default()
    };
    for (key, s) in locals {
        let i = table.syms.len() as u32;
        match key {
            Local::Symbol(id) => table.index.insert(id, i),
            Local::SectionLabel(section) => table.labels.insert(section, i),
        };
        table.syms.push(s);
    }
    for (id, s) in externals.into_iter().chain(undefined) {
        table.index.insert(id, table.syms.len() as u32);
        table.syms.push(s);
    }
    table
}

/// A local symbol table entry: one of the assembler's symbols, or a section's
/// `ltmpN` label.
enum Local {
    Symbol(SymbolId),
    SectionLabel(SectionId),
}

/// The symbol table, and where in it each symbol and section label went.
#[derive(Default)]
struct Symtab {
    syms: Vec<OutSym>,
    index: HashMap<SymbolId, u32>,
    labels: HashMap<SectionId, u32>,
    /// How many locals, defined externals and undefined symbols, in order.
    counts: (u32, u32, u32),
}

/// `N_EXT`, plus `N_PEXT` for a `.private_extern` symbol, which rsasm records
/// as hidden visibility since the two mean the same thing.
fn ext_bits(global: bool, visibility: Visibility) -> u8 {
    if !global {
        return 0;
    }
    match visibility {
        Visibility::Hidden | Visibility::Internal => N_EXT | N_PEXT,
        _ => N_EXT,
    }
}

/// The description bits a `.weak` symbol gets: a definition is weak, a
/// reference to one elsewhere may be missing at run time.
fn weak_bits(binding: Binding, defined: bool) -> u16 {
    match (binding, defined) {
        (Binding::Weak, true) => N_WEAK_DEF,
        (Binding::Weak, false) => N_WEAK_REF,
        _ => 0,
    }
}

// ---- the file itself --------------------------------------------------------

/// A growable little-endian byte sink. Both Mach-O machines here are
/// little-endian, and the format's own fields follow the machine.
#[derive(Default)]
struct Buf {
    out: Vec<u8>,
}

impl Buf {
    fn u32(&mut self, v: u32) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }
    fn u8(&mut self, v: u8) {
        self.out.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }
    /// An address or a size, as wide as the object makes it.
    fn word(&mut self, wide: bool, v: u64) {
        if wide {
            self.u64(v);
        } else {
            self.u32(v as u32);
        }
    }
    /// A fixed-width name, NUL-padded and truncated as Mach-O stores them.
    fn name16(&mut self, s: &str) {
        let mut buf = [0u8; 16];
        let b = s.as_bytes();
        let n = b.len().min(16);
        buf[..n].copy_from_slice(&b[..n]);
        self.out.extend_from_slice(&buf);
    }
    fn pad_to(&mut self, align: u64) {
        while !(self.out.len() as u64).is_multiple_of(align) {
            self.out.push(0);
        }
    }
    fn len(&self) -> u64 {
        self.out.len() as u64
    }
}

/// The bytes one `LC_LINKER_OPTION` takes: the command, the count, and each
/// word with its terminator, rounded up to a pointer.
fn linker_option_size(words: &[String], align: u32) -> u32 {
    let strings: usize = words.iter().map(|w| w.len() + 1).sum();
    (LINKER_OPTION_COMMAND_SIZE + strings as u32).next_multiple_of(align)
}

fn write(
    asm: &Assembler,
    cpu: Cpu,
    secs: &[Sec],
    syms: &[OutSym],
    counts: (u32, u32, u32),
    regions: &[(u32, u16, u16)],
    indirect: &Indirect,
) -> Vec<u8> {
    // A 64-bit object is padded to a pointer throughout -- the file after
    // the sections, each `LC_LINKER_OPTION` and the string table that ends
    // the file -- and a 32-bit one to four bytes.
    let wide = cpu.wide();
    let pad = if wide { 8 } else { 4 };
    let strings = string_table(syms, pad as usize);

    // The deployment target the target triple named, which a
    // `.build_version` or `.*_version_min` in the source replaces.
    let version = asm.macho.build_version.or(asm.macho.deployment.version);
    // An object with no symbols has no symbol table, nor the commands that
    // would describe one.
    let has_symtab = !syms.is_empty();
    let data_in_code = !regions.is_empty();
    let options = &asm.macho.linker_options;
    let ncmds = 1
        + version.is_some() as u32
        + data_in_code as u32
        + 2 * has_symtab as u32
        + options.len() as u32;
    let (header_size, segment_size, section_size, nlist_size, segment_command) = if wide {
        (
            HEADER_64_SIZE,
            SEGMENT_COMMAND_64_SIZE,
            SECTION_64_SIZE,
            NLIST_64_SIZE,
            LC_SEGMENT_64,
        )
    } else {
        (
            HEADER_SIZE,
            SEGMENT_COMMAND_SIZE,
            SECTION_SIZE,
            NLIST_SIZE,
            LC_SEGMENT,
        )
    };
    let sizeofcmds = segment_size
        + section_size * secs.len() as u32
        + if data_in_code {
            LINKEDIT_DATA_COMMAND_SIZE
        } else {
            0
        }
        + if has_symtab {
            SYMTAB_COMMAND_SIZE + DYSYMTAB_COMMAND_SIZE
        } else {
            0
        }
        + match version {
            Some(v) if v.command == LC_BUILD_VERSION => BUILD_VERSION_COMMAND_SIZE,
            Some(_) => VERSION_MIN_COMMAND_SIZE,
            None => 0,
        }
        + options
            .iter()
            .map(|w| linker_option_size(w, pad))
            .sum::<u32>();

    // The file is laid out before anything is written: every load command
    // holds an offset into what comes after it.
    let data_start = (header_size + sizeofcmds) as u64;
    let file_size: u64 = secs
        .iter()
        .filter(|s| !s.zerofill)
        .map(|s| s.addr + s.size)
        .max()
        .unwrap_or(0);
    let vm_size: u64 = secs.iter().map(|s| s.addr + s.size).max().unwrap_or(0);
    let reloc_start = (data_start + file_size).next_multiple_of(pad as u64);
    let mut off = reloc_start;
    let mut reloc_off = Vec::with_capacity(secs.len());
    for s in secs {
        reloc_off.push(off);
        off += s.relocs.len() as u64 * RELOCATION_SIZE;
    }
    let dataoff = off;
    let indirectoff = dataoff + regions.len() as u64 * DATA_IN_CODE_ENTRY_SIZE;
    let symoff = indirectoff + indirect.entries.len() as u64 * INDIRECT_SYMBOL_SIZE;
    let stroff = symoff + syms.len() as u64 * nlist_size;

    let mut b = Buf::default();
    let (cputype, cpusubtype) = cpu.header();
    b.u32(if wide { MH_MAGIC_64 } else { MH_MAGIC });
    b.u32(cputype);
    b.u32(cpusubtype);
    b.u32(MH_OBJECT);
    b.u32(ncmds);
    b.u32(sizeofcmds);
    b.u32(if asm.macho.subsections_via_symbols {
        MH_SUBSECTIONS_VIA_SYMBOLS
    } else {
        0
    });
    if wide {
        b.u32(0); // reserved, which only the wider header has
    }

    // ---- the one segment, with every section ------------------------------
    b.u32(segment_command);
    b.u32(segment_size + section_size * secs.len() as u32);
    b.name16(""); // an object's one segment has no name
    b.word(wide, 0); // vmaddr
    b.word(wide, vm_size);
    b.word(wide, data_start);
    b.word(wide, file_size);
    b.u32(7); // maxprot: rwx
    b.u32(7); // initprot
    b.u32(secs.len() as u32);
    b.u32(0); // flags
    for (i, s) in secs.iter().enumerate() {
        b.name16(&s.section);
        b.name16(&s.segment);
        b.word(wide, s.addr);
        b.word(wide, s.size);
        b.u32(if s.zerofill {
            0
        } else {
            (data_start + s.addr) as u32
        });
        b.u32(s.align);
        b.u32(if s.relocs.is_empty() {
            0
        } else {
            reloc_off[i] as u32
        });
        b.u32(s.relocs.len() as u32);
        b.u32(s.flags);
        b.u32(indirect.bases.get(&s.id).copied().unwrap_or(0)); // reserved1
        b.u32(s.reserved2);
        if wide {
            b.u32(0); // reserved3, which only `section_64` has
        }
    }

    if let Some(v) = version {
        b.u32(v.command);
        if v.command == LC_BUILD_VERSION {
            b.u32(BUILD_VERSION_COMMAND_SIZE);
            b.u32(v.platform);
            b.u32(v.minos);
            b.u32(v.sdk);
            b.u32(0); // ntools
        } else {
            b.u32(VERSION_MIN_COMMAND_SIZE);
            b.u32(v.minos);
            b.u32(v.sdk);
        }
    }

    if data_in_code {
        b.u32(LC_DATA_IN_CODE);
        b.u32(LINKEDIT_DATA_COMMAND_SIZE);
        b.u32(dataoff as u32);
        b.u32((regions.len() as u64 * DATA_IN_CODE_ENTRY_SIZE) as u32);
    }

    if has_symtab {
        b.u32(LC_SYMTAB);
        b.u32(SYMTAB_COMMAND_SIZE);
        b.u32(symoff as u32);
        b.u32(syms.len() as u32);
        b.u32(stroff as u32);
        b.u32(strings.bytes.len() as u32);

        b.u32(LC_DYSYMTAB);
        b.u32(DYSYMTAB_COMMAND_SIZE);
        let (nlocal, nextdef, nundef) = counts;
        b.u32(0); // ilocalsym
        b.u32(nlocal);
        b.u32(nlocal); // iextdefsym
        b.u32(nextdef);
        b.u32(nlocal + nextdef); // iundefsym
        b.u32(nundef);
        for _ in 0..6 {
            b.u32(0); // the tables an assembler never writes
        }
        b.u32(if indirect.entries.is_empty() {
            0
        } else {
            indirectoff as u32
        });
        b.u32(indirect.entries.len() as u32);
        for _ in 0..4 {
            b.u32(0); // the relocations a linker moves into the __LINKEDIT
        }
    }

    // The linker options come after the symbol table's commands, as llvm-mc
    // writes them.
    for words in options {
        b.u32(LC_LINKER_OPTION);
        b.u32(linker_option_size(words, pad));
        b.u32(words.len() as u32);
        let end = b.len() + u64::from(linker_option_size(words, pad) - LINKER_OPTION_COMMAND_SIZE);
        for w in words {
            b.out.extend_from_slice(w.as_bytes());
            b.u8(0);
        }
        while b.len() < end {
            b.u8(0);
        }
    }

    debug_assert_eq!(b.len(), data_start, "load commands must fill the header");

    // ---- section contents -------------------------------------------------
    for s in secs.iter().filter(|s| !s.zerofill) {
        while b.len() < data_start + s.addr {
            b.u8(0);
        }
        b.out.extend_from_slice(&s.bytes);
    }
    b.pad_to(pad as u64);

    for s in secs {
        for r in &s.relocs {
            let (first, second) = r.words();
            b.u32(first);
            b.u32(second);
        }
    }

    for &(offset, length, kind) in regions {
        b.u32(offset);
        b.u16(length);
        b.u16(kind);
    }

    for &entry in &indirect.entries {
        b.u32(entry);
    }

    for s in syms {
        b.u32(strings.offset(&s.name));
        b.u8(s.n_type);
        b.u8(s.n_sect);
        b.u16(s.n_desc);
        b.word(wide, s.n_value);
    }
    if has_symtab {
        b.out.extend_from_slice(&strings.bytes);
    }
    b.out
}

/// The string table, built the way llvm-mc builds it: the names sorted so
/// that one which is a suffix of another follows it, and shares its tail.
struct Strings {
    bytes: Vec<u8>,
    offsets: HashMap<String, u32>,
}

impl Strings {
    fn offset(&self, name: &str) -> u32 {
        self.offsets.get(name).copied().unwrap_or(0)
    }
}

fn string_table(syms: &[OutSym], pad: usize) -> Strings {
    let mut names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    // Descending by the reversed name, which puts every suffix right after
    // the name it can share a tail with.
    names.sort_by(|a, b| {
        let (ra, rb): (Vec<u8>, Vec<u8>) = (a.bytes().rev().collect(), b.bytes().rev().collect());
        rb.cmp(&ra)
    });

    let mut bytes = vec![0u8];
    let mut offsets = HashMap::new();
    let mut previous: Option<(&str, u32)> = None;
    for name in names {
        if name.is_empty() {
            offsets.insert(String::new(), 0);
            continue;
        }
        if let Some((prev, at)) = previous
            && prev.ends_with(name)
        {
            offsets.insert(name.to_string(), at + (prev.len() - name.len()) as u32);
            continue;
        }
        let at = bytes.len() as u32;
        bytes.extend_from_slice(name.as_bytes());
        bytes.push(0);
        offsets.insert(name.to_string(), at);
        previous = Some((name, at));
    }
    // Padded to a word, as the table ends the file.
    while !bytes.len().is_multiple_of(pad) {
        bytes.push(0);
    }
    Strings { bytes, offsets }
}
