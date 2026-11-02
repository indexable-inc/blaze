//! What the linker's core and an object format's writer share (docs/v2/plan/lanes/link/notes.md).
//!
//! The core is the same for every format: it interns symbols, places pieces in regions of
//! one address space, makes the jump table, the GOT and the PLT, and applies every
//! relocation. A format ([`Writer`]: ELF, Mach-O, PE, wasm) says where regions go
//! ([`Plan`]), reads the format's native inputs ([`NativeFile`]), names the symbols the
//! linker defines ([`Special`]), fills the regions only it knows the contents of (dynamic
//! tables, unwind indexes, code signatures: [`Class::Synthetic`]), and writes the file's
//! headers around the placed regions ([`Writer::file`]).
//!
//! Addresses are virtual addresses as the file states them, before the loader slides the
//! image: [`Plan::base`] is the address of the image's first byte.

use std::ops::Range;
use std::sync::Arc;

use blaze_codegen::Arch;

use crate::cow::CowBytes;
use crate::{Format, LinkError, Output};

/// A [`Class::Named`] section's name: a format's own (`"__LD,__compact_unwind"`) borrowed,
/// one read from an input made once and shared, so a class clones without allocating. It
/// compares, hashes, orders and prints as its text, whichever it is.
#[derive(Clone)]
pub(crate) enum SectionName {
    Static(&'static str),
    Shared(Arc<str>),
}

impl SectionName {
    /// A name read from an input.
    pub fn new(name: &str) -> SectionName {
        // alloc: once per named section read, shared by every clone after
        SectionName::Shared(Arc::from(name))
    }

    #[inline]
    pub fn as_str(&self) -> &str {
        match self {
            SectionName::Static(it) => it,
            SectionName::Shared(it) => it,
        }
    }
}

impl From<&'static str> for SectionName {
    fn from(name: &'static str) -> SectionName {
        SectionName::Static(name)
    }
}

impl From<String> for SectionName {
    fn from(name: String) -> SectionName {
        // alloc: the edge: a name already made, moved into its one shared buffer
        SectionName::Shared(Arc::from(name))
    }
}

impl std::ops::Deref for SectionName {
    type Target = str;

    #[inline]
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl PartialEq for SectionName {
    fn eq(&self, other: &SectionName) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for SectionName {}

impl PartialEq<str> for SectionName {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for SectionName {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<String> for SectionName {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<SectionName> for str {
    fn eq(&self, other: &SectionName) -> bool {
        self == other.as_str()
    }
}

impl PartialEq<SectionName> for &str {
    fn eq(&self, other: &SectionName) -> bool {
        *self == other.as_str()
    }
}

impl PartialEq<SectionName> for String {
    fn eq(&self, other: &SectionName) -> bool {
        self == other.as_str()
    }
}

impl std::hash::Hash for SectionName {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl PartialOrd for SectionName {
    fn partial_cmp(&self, other: &SectionName) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SectionName {
    fn cmp(&self, other: &SectionName) -> std::cmp::Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl std::fmt::Debug for SectionName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.as_str(), f)
    }
}

impl std::fmt::Display for SectionName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a region holds. A region is one range of the address space with one protection;
/// the format writer orders them ([`Writer::order`]) and names them in the file.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub(crate) enum Class {
    /// Code of a full link.
    Text,
    /// Code placed by incremental links since the last full one, and by live patches: kept
    /// apart from the full link's code, which they leave as it is.
    Patch,
    /// The jump table's stubs: one per [`crate::Slot`].
    Stubs,
    /// Stubs that jump through a GOT cell to an imported function.
    Plt,
    /// Range-extension veneers for AArch64 branches that can't reach their target.
    Veneers,
    Rodata,
    /// Read-only once the loader has relocated it: constant data holding addresses (vtables,
    /// panic locations, constant statics with pointers), ELF's `.data.rel.ro`, Mach-O's
    /// `__DATA_CONST,__const`, PE's `.rdata`.
    RelRo,
    /// Call frame information (`.eh_frame`, `__eh_frame`) that the core assembles.
    EhFrame,
    Data,
    /// The jump table's cells, and the cell that points at the current table.
    Table,
    /// Address cells: of imports, of local symbols reached through the GOT, of TLS offsets
    /// and descriptors.
    Got,
    /// Pointers the loader calls at start (`.init_array`, `__mod_init_func`), in order.
    InitArray,
    FiniArray,
    /// ELF `.preinit_array`: called before every `.init_array` entry and every shared
    /// library's initializers; only a dev run's start hook goes here ([`crate::DevRun`]).
    PreinitArray,
    /// A dev run's reserved address range ([`crate::DevRun`]): no bytes, no access, not in
    /// the file.
    Reserve(Reserve),
    /// Data placed by incremental links and live patches, as [`Class::Patch`] is for code.
    PatchData,
    /// A section a static or a native object names (`#[link_section]`), kept whole and in
    /// input order so the program can walk it between its start and stop symbols.
    Named { name: SectionName, write: bool, exec: bool },
    /// Mach-O thread-local variable descriptors (`__thread_vars`): three words each, the
    /// `_tlv_bootstrap` thunk (bound), a key (0) and the variable's offset in the template.
    /// The core makes one for each thread-local static when [`Plan::tls_descriptors`].
    ThreadVars,
    /// The thread-local template: initialized, then zeroed.
    Tdata,
    Tbss,
    /// Zeroed data, not in the file.
    Bss,
    /// Bytes only the writer knows how to make, from the placed image: named by the writer.
    Synthetic(&'static str),
}

/// The reservations of a dev run, in address order.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub(crate) enum Reserve {
    Arena,
    Cells,
    Fixed,
}

/// Whether a class is a dev run's reservation ([`Class::Reserve`]): no bytes in memory or in
/// the file, unlike [`Class::Bss`], which is zeroed memory. Its region's `size` is 0 and its
/// `capacity` the range reserved.
#[inline]
pub(crate) fn reserved(class: &Class) -> bool {
    matches!(class, Class::Reserve(_))
}

/// What a region may be accessed as.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct Prot {
    pub read: bool,
    pub write: bool,
    pub exec: bool,
}

impl Prot {
    pub const RX: Prot = Prot { read: true, write: false, exec: true };
    pub const R: Prot = Prot { read: true, write: false, exec: false };
    pub const RW: Prot = Prot { read: true, write: true, exec: false };
    pub const NONE: Prot = Prot { read: false, write: false, exec: false };
}

/// How a format lays an image out.
#[derive(Clone, Debug)]
pub(crate) struct Plan {
    /// The address of the image's first byte (its headers).
    pub base: u64,
    /// Bytes at the start of the first segment for the headers ([`Writer::file`] writes
    /// them; it may not need more than this).
    pub headers: u64,
    /// The alignment of a change of protection: the largest page size of the target.
    pub page: u64,
    /// Whether code is reached through a function table by index (wasm) rather than at
    /// addresses: then each code piece's address is its function index.
    pub indexed_code: bool,
    /// Whether the GOT's import cells are grouped by library with a zero cell after each
    /// group (a PE import address table).
    pub got_by_library: bool,
    /// Whether `#[thread_local]` statics are reached through descriptors (Mach-O's TLV) rather
    /// than at an offset from the thread pointer.
    pub tls_descriptors: bool,
}

/// A symbol the linker defines, by name ([`Writer::special`]).
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Special {
    /// The first byte of the image.
    ImageBase,
    /// The start or the end of the regions of a class; an error when there is none.
    Start(Class),
    Stop(Class),
    /// The start or the end of the [`Class::Named`] regions of this name, whatever their
    /// flags; an error naming the section when there is none (ELF `__start_X`, #1496).
    Section { name: SectionName, end: bool },
    /// An address the writer computes from the placed regions (ELF's `_end`, `etext`,
    /// `__bss_start`, `_DYNAMIC`).
    Placed(fn(&[Region]) -> u64),
    /// An absolute value.
    Absolute(u64),
    /// The descriptor a live program finds its jump table by (`blaze_live::patch`).
    LiveDescriptor,
    /// The address of the GOT cell of the symbol named so, resolved as any reference to it
    /// is: an import's import address table entry, or a cell holding a local symbol's
    /// address (PE's `__imp_X`; link.exe's LNK4217 case). The symbol's name is the special's
    /// own from byte `from` (6 for `__imp_X`), so the writer makes no string for it.
    Got { from: u32 },
}

impl Special {
    /// Of a [`Special::Got`] that `name` named, the name of the symbol it is the cell of.
    #[inline]
    pub fn got_target<'n>(&self, name: &'n str) -> Option<&'n str> {
        match self {
            Special::Got { from } => name.get(*from as usize..),
            _ => None,
        }
    }
}

/// How a relocation is written. Every format's relocation types are read into these, and
/// codegen's [`blaze_codegen::RelocKind`]s map onto them; `S` is the target's address, `A`
/// the addend, `P` the place's address, `G` the address of the target's GOT cell, `M` the
/// address of [`Fixup::minus`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Apply {
    /// `S + A`, 64 bits; the loader slides it ([`Placed::rebase`]) or binds it.
    Abs64,
    /// `S + A`, 32 bits, unsigned or signed: only in an image the loader never slides.
    Abs32,
    Abs32S,
    /// `S + A - P`.
    Rel64,
    Rel32,
    /// `S - M + A`.
    Delta64,
    Delta32,
    /// `S + A - base`: an image-relative address (PE's `ADDR32NB`, unwind tables).
    ImageRel32,
    /// The offset of `S + A` in its output section (PE's `SECREL`), and its section's index.
    SecRel32,
    Section16,
    /// A call or jump to `S + A`: through the PLT when `S` is imported. x86-64 `rel32`.
    X86Branch32,
    /// `G + A - P`: the GOT cell of `S` (x86-64 `GOTPCREL`, `GOTPCRELX`, `REX_GOTPCRELX`).
    X86GotPcRel32,
    /// `S - end of the TLS block + A`: local exec (`TPOFF32`).
    X86TpOff32,
    /// The GOT cell holding `S`'s offset from the thread pointer (`GOTTPOFF`).
    X86GotTpOff32,
    /// The GOT pair of `S`'s module and offset, for `__tls_get_addr` (`TLSGD`).
    X86TlsGd32,
    /// `S`'s offset in its module's TLS block (`DTPOFF32`, `DTPOFF64`).
    DtpOff32,
    DtpOff64,
    /// AArch64 `BL`, `B`: through the PLT when `S` is imported, a veneer when out of reach.
    A64Call26,
    A64Jump26,
    /// AArch64 `ADRP` of `S + A`'s page, and `ADR` of `S + A`.
    A64Page21,
    A64Adr21,
    /// The low 12 bits of `S + A`, shifted right by `shift` (`ADD` 0, `LDR`/`STR` 0 to 4).
    A64Lo12 { shift: u8 },
    A64CondBr19,
    A64TstBr14,
    /// `MOVZ`/`MOVK` of 16 bits of `S + A` at `group * 16`.
    A64MovW { group: u8 },
    /// The GOT cell of `S`: its page, its low 12 bits for an `LDR` (Mach-O's `GOT_LOAD_*`).
    A64GotPage21,
    A64GotLo12,
    /// `G + A - P`, 32 bits (Mach-O's `POINTER_TO_GOT`).
    GotRel32,
    /// Local exec: the offset of `S` from the thread pointer, high and low 12 bits.
    A64TpHi12,
    A64TpLo12,
    /// Initial exec: the GOT cell holding the offset (`TLSIE_*`).
    A64TlsIePage21,
    A64TlsIeLo12,
    /// A TLS descriptor's GOT pair (`TLSDESC_*`); the call needs nothing written.
    A64TlsDescPage21,
    A64TlsDescLo12,
    A64TlsDescAdd12,
    A64TlsDescCall,
    /// Mach-O TLV (`TLVP_LOAD_*`): the page and low 12 bits (an `LDR`) of a GOT cell holding
    /// the address of `S`'s descriptor (`S` itself for a native one in `__thread_vars`, the one
    /// the core makes in [`Class::ThreadVars`] for a thread-local static).
    TlvPage21,
    TlvLo12,
    /// x86-64 Mach-O TLV: `rel32` to that cell.
    TlvRel32,
    /// The thread-local offset of `S + A` codegen asks for ([`blaze_codegen::RelocKind::TlsOffset`]):
    /// 32 bits on x86-64 (`TPOFF32`), a 64-bit literal on AArch64.
    TlsOffset,
    /// `S`'s offset from the thread pointer, 64 bits: a GOT cell of an initial-exec access
    /// to a thread-local of the image. The core's own; no reader emits it.
    TpOff64,
    /// A relocation only the format's writer applies ([`Writer::apply`]), by its own type.
    Native(u32),
}

/// One relocation of a piece.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Fixup {
    /// From the start of the piece.
    pub at: u64,
    pub apply: Apply,
    pub target: Sym,
    pub addend: i64,
    /// [`Apply::Delta64`], [`Apply::Delta32`]: the symbol subtracted.
    pub minus: Option<Sym>,
}

/// A symbol of the image: a dense index into its symbol table. One run's state: never in a
/// key or an output.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub(crate) struct Sym(pub u32);

/// A native input as a format reads it.
///
/// Readers copy nothing they can point at: a section's bytes are a range of the input
/// ([`Data::Input`]), every name is a [`Name`] in the file's one [`Names`] arena, relocations
/// and group members are ranges of one column per object. The core checks every range once
/// when it takes the file ([`crate::native`]) and reads them in place after that.
pub(crate) enum NativeFile {
    Object(Object),
    Archive(Archive),
    Shared(Shared),
}

/// A name in a file's [`Names`]: `len` bytes from `at`. Made only by [`Names::push`], so it
/// always starts and ends on a character boundary of the arena it came from.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct Name {
    at: u32,
    len: u32,
}

const _: () = assert!(std::mem::size_of::<Name>() == 8);

/// Every name of one native file, one after another in one allocation. Not `Clone`: the
/// big model types are borrowed or moved, never copied.
#[derive(Default, PartialEq, Eq, Debug)]
pub(crate) struct Names(String);

impl Names {
    pub fn with_capacity(bytes: usize) -> Names {
        Names(String::with_capacity(bytes))
    }

    /// Room for `bytes` more of names, for a reader that learns their size only after its
    /// first pushes.
    pub fn reserve(&mut self, bytes: usize) {
        self.0.reserve(bytes);
    }

    /// Adds `name`; an error past 4 GiB of names in one file.
    pub fn push(&mut self, name: &str) -> Result<Name, String> {
        let at = u32::try_from(self.0.len()).ok();
        let len = u32::try_from(name.len()).ok();
        match (at, len) {
            (Some(at), Some(len)) if at.checked_add(len).is_some() => {
                self.0.push_str(name);
                Ok(Name { at, len })
            }
            // alloc: error path
            _ => Err("more than 4 GiB of names in one file".to_owned()),
        }
    }

    /// The name, when it is one of these names.
    pub fn get(&self, name: Name) -> Option<&str> {
        self.0.get(name.at as usize..name.at as usize + name.len as usize)
    }

    /// The name; `name` must be one [`Names::push`] gave for these names (the core checks
    /// every name of a file once, when it takes the file).
    pub fn at(&self, name: Name) -> &str {
        &self.0[name.at as usize..name.at as usize + name.len as usize]
    }

    /// `name`'s bytes from `from`, a name inside it: what a string table indexed by offset (a
    /// BSD `__.SYMDEF`, ELF's tail-merged `.strtab`) names, so each string is pushed once. None
    /// past its end or off a character boundary.
    pub fn suffix(&self, name: Name, from: u32) -> Option<Name> {
        let rest = self.get(name)?.get(from as usize..)?;
        Some(Name { at: name.at + from, len: rest.len() as u32 })
    }

    pub fn bytes(&self) -> usize {
        self.0.len()
    }

    /// Appends `other`'s names, returning the offset they start at here: a [`Name`] of `other`
    /// [`Name::shifted`] by it names the same text in `self`. An error past 4 GiB.
    pub fn append(&mut self, other: &Names) -> Result<u32, String> {
        let at = u32::try_from(self.0.len()).ok();
        let len = u32::try_from(other.0.len()).ok();
        match (at, len) {
            (Some(at), Some(len)) if at.checked_add(len).is_some() => {
                self.0.push_str(&other.0);
                Ok(at)
            }
            // alloc: error path
            _ => Err("more than 4 GiB of names in one file".to_owned()),
        }
    }
}

impl Name {
    /// The same name in the arena it was appended to: `by` is what [`Names::append`] returned
    /// for the arena this name was made in. `other` is appended whole, so each of its character
    /// boundaries is one of the arena's.
    pub fn shifted(self, by: u32) -> Name {
        Name { at: self.at + by, len: self.len }
    }
}

/// A relocatable object (`.o`, `.obj`, a member of an archive).
#[derive(Default, Debug)]
pub(crate) struct Object {
    pub sections: Vec<Section>,
    pub symbols: Vec<Symbol>,
    /// Every section's relocations, each section's a run of them ([`Section::fixups`]).
    pub fixups: Vec<NativeFixup>,
    /// COMDAT groups: of each name only the first object's sections are kept.
    pub groups: Vec<Group>,
    /// Every group's sections, each group's a run of them ([`Group::sections`]).
    pub grouped: Vec<u32>,
    /// Sections kept exactly when another of the object's is, and never for themselves
    /// (`SHF_LINK_ORDER`, an associative COMDAT): at most one each.
    pub ties: Vec<Tie>,
    /// Sections kept only when a section they refer to is kept (`S_ATTR_LIVE_SUPPORT`), and
    /// never for themselves.
    pub live_support: Vec<u32>,
    /// The names of the symbols, sections and groups.
    pub names: Names,
    /// wasm: what an undefined function is imported as when nothing defines it, by symbol
    /// index. Other formats leave it empty.
    pub imports: Vec<(u32, WasmImport)>,
    /// Every byte the reader made rather than found in the input (a header it prepends, a
    /// section it rewrote), in one buffer per object: [`Data::Made`] ranges of it. Empty for a
    /// reader that makes none.
    pub made: Vec<u8>,
    /// Libraries the object asks for (`/DEFAULTLIB` of a COFF object, `LC_LINKER_OPTION`).
    pub libraries: Vec<String>,
    /// What a COFF object's `.drectve` asks of the link beyond its libraries.
    pub directives: Vec<Directive>,
}

impl Object {
    pub fn name(&self, name: Name) -> &str {
        self.names.at(name)
    }

    /// A section's relocations.
    pub fn section_fixups(&self, section: &Section) -> &[NativeFixup] {
        &self.fixups[section.fixups.start as usize..section.fixups.end as usize]
    }

    /// A group's sections.
    pub fn group_sections(&self, group: &Group) -> &[u32] {
        &self.grouped[group.sections.start as usize..group.sections.end as usize]
    }

    /// `data`'s bytes, `data` being one of this object's and `input` the slice it was read
    /// from: [`Data::bytes`], with [`Data::Made`] read from [`Object::made`].
    pub fn section_bytes<'b>(&'b self, data: &'b Data, input: &'b [u8]) -> Result<Option<&'b [u8]>, String> {
        match data {
            Data::Made(range) => slice(&self.made, range, "reader's made bytes"),
            _ => data.bytes(input),
        }
    }
}

/// What a COFF object's `.drectve` asks of the link beyond [`Object::libraries`].
#[derive(PartialEq, Eq, Debug)]
pub(crate) enum Directive {
    /// `/INCLUDE:sym`: the link defines `sym` (pulling archive members for it) and keeps it.
    Include(String),
    /// `/EXPORT:name[=symbol][,DATA]`: the image exports `symbol` under `name`.
    Export { name: String, symbol: String, data: bool },
    /// `/FAILIFMISMATCH:key=value`: every object of the link naming `key` names `value`; an
    /// error naming both objects otherwise (LNK2038: mixing CRTs or `_ITERATOR_DEBUG_LEVEL`).
    Mismatch { key: String, value: String },
    /// `/NODEFAULTLIB[:lib]`: the default library `lib` (every one when `None`) is neither put
    /// on the line by [`crate::target::locate`] nor required by the engine for any object or
    /// member it loads.
    NoDefaultLib(Option<String>),
}

#[derive(Debug)]
pub(crate) struct Section {
    pub name: Name,
    pub what: SectionKind,
    pub align: u64,
    pub data: Data,
    /// Its relocations: this run of [`Object::fixups`].
    pub fixups: Range<u32>,
    /// Kept whatever refers to it (`SHF_GNU_RETAIN`, `S_ATTR_NO_DEAD_STRIP`).
    pub retain: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum SectionKind {
    /// Placed in this class, at this rank ([`Writer::section`]).
    Alloc(Class, u32),
    /// Call frame information, merged into [`Class::EhFrame`] by the core.
    EhFrame,
    /// Format-specific unwind input the writer consumes (`__compact_unwind`, `.pdata`,
    /// `.xdata`): placed in this class, and handed to [`Writer::synthesize`].
    Unwind(Class),
    /// Not loaded: debug info, notes, the linker's directives. Left out of the image.
    Dropped,
}

#[derive(PartialEq, Eq, Debug)]
pub(crate) enum Data {
    /// `bytes[range]` of the slice handed to [`Writer::read`] (for an archive member, the
    /// member's own slice): the section as the file has it, never copied.
    Input(Range<u64>),
    /// Bytes the reader made: ones the file does not hold as they are (a header it
    /// prepends, a section it rewrote).
    Bytes(Vec<u8>),
    /// `made[range]` of the object's [`Object::made`]: bytes the reader made, all of an
    /// object's in one buffer.
    Made(Range<u64>),
    Zero(u64),
}

impl Data {
    pub fn len(&self) -> u64 {
        match self {
            Data::Input(range) | Data::Made(range) => range.end.saturating_sub(range.start),
            Data::Bytes(bytes) => bytes.len() as u64,
            Data::Zero(size) => *size,
        }
    }

    /// Its bytes, `input` being the slice the file was read from: none for zeroed data, an
    /// error for a range past the input's end. [`Data::Made`] is read with its object
    /// ([`Object::section_bytes`]); here it is an error.
    pub fn bytes<'b>(&'b self, input: &'b [u8]) -> Result<Option<&'b [u8]>, String> {
        match self {
            Data::Input(range) => slice(input, range, "input"),
            Data::Made(range) => {
                // alloc: error path
                Err(format!("bytes {range:?} the reader made, read without their object (`Object::section_bytes`)"))
            }
            Data::Bytes(bytes) => Ok(Some(bytes)),
            Data::Zero(_) => Ok(None),
        }
    }
}

/// `from[range]`, or an error naming `what` when it is past the end.
fn slice<'b>(from: &'b [u8], range: &Range<u64>, what: &str) -> Result<Option<&'b [u8]>, String> {
    usize::try_from(range.start)
        .ok()
        .zip(usize::try_from(range.end).ok())
        .and_then(|(start, end)| from.get(start..end))
        .map(Some)
        // alloc: error path
        .ok_or_else(|| format!("bytes {range:?} past the {what}'s {} bytes", from.len()))
}

/// wasm: what an undefined function is imported as, read in place: its module and field
/// names in the object's [`Names`], its type the encoded function type (`0x60` …) as
/// `bytes[ty]` of the input, as [`Data::Input`] ranges are. The reader checks the encoding;
/// the core decodes it into a [`blaze_codegen::wasm::Import`] once per import per graph built.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct WasmImport {
    pub module: Name,
    pub field: Name,
    pub ty: Range<u64>,
}

impl WasmImport {
    /// Its encoded function type, `input` being the slice its object was read from.
    pub fn type_bytes<'b>(&self, input: &'b [u8]) -> Result<&'b [u8], String> {
        slice(input, &self.ty, "input").map(Option::unwrap_or_default)
    }
}

/// A relocation of a native section, by the object's symbol indexes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct NativeFixup {
    pub at: u64,
    pub apply: Apply,
    pub symbol: u32,
    pub addend: i64,
    pub minus: Option<u32>,
}

const _: () = assert!(std::mem::size_of::<NativeFixup>() <= 40);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Symbol {
    pub name: Name,
    pub def: SymbolDef,
    pub binding: Binding,
    pub kind: SymbolKind,
    /// Not exported from a shared library (`STV_HIDDEN`, a private extern).
    pub hidden: bool,
}

const _: () = assert!(std::mem::size_of::<Symbol>() <= 48);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SymbolDef {
    Undefined,
    /// At `value` from the start of the object's section `section`.
    Section { section: u32, value: u64, size: u64 },
    /// A tentative definition, merged by name; the largest wins.
    Common { size: u64, align: u64 },
    Absolute(u64),
    /// Undefined here; when the link leaves the name undefined, it is the object's symbol
    /// `symbol` (COFF weak externals, `/ALTERNATENAME:from=to`). Archives are searched for it
    /// on this object's account only when `search` (`IMAGE_WEAK_EXTERN_SEARCH_LIBRARY`,
    /// `ALIAS`); another object's reference still may. `symbol` is resolved (and an error
    /// when undefined) only when the fallback is taken.
    Fallback { symbol: u32, search: bool },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Binding {
    Local,
    Global,
    Weak,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SymbolKind {
    Function,
    Data,
    Tls,
    /// The section's own symbol: its start.
    Section,
    Other,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Group {
    pub name: Name,
    /// Its sections: this run of [`Object::grouped`].
    pub sections: Range<u32>,
    /// Of this name the group whose first section is largest is kept, not the first one
    /// (`IMAGE_COMDAT_SELECT_LARGEST`: MSVC's vftables with or without their RTTI locator).
    pub largest: bool,
}

/// Section `section` tied to section `to` of the same object ([`Object::ties`]): ELF
/// `SHF_LINK_ORDER` to its `sh_link`, a COFF associative COMDAT to its root.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Tie {
    pub section: u32,
    pub to: u32,
    /// Placed among the sections of its class in the order of the sections they are tied to,
    /// not their own (`SHF_LINK_ORDER`).
    pub ordered: bool,
}

/// An archive: its members are read only when one defines a symbol the link needs, as a
/// linker takes them, unless the archive is taken whole.
#[derive(Debug)]
pub(crate) struct Archive {
    pub members: Vec<Member>,
    /// Each symbol the archive's index names and the member defining it, in the index's
    /// order.
    pub index: Vec<(Name, u32)>,
    /// The names of the index's symbols and of the members.
    pub names: Names,
}

impl Archive {
    pub fn name(&self, name: Name) -> &str {
        self.names.at(name)
    }
}

#[derive(Debug)]
pub(crate) struct Member {
    pub name: Name,
    pub bytes: Range<usize>,
}

/// A shared library: what the image may import from it.
#[derive(Debug)]
pub(crate) struct Shared {
    /// What the loader finds it by: the ELF `DT_SONAME`, the Mach-O install name, the DLL.
    pub name: String,
    pub symbols: Vec<SharedSymbol>,
    /// The symbols' names.
    pub names: Names,
    /// The versions symbols are defined at, each once ([`SharedSymbol::version`]): names in
    /// [`Shared::names`].
    pub versions: Vec<Name>,
    /// More shared libraries it stands for: a linker script's `GROUP`, a `.tbd`'s
    /// re-exports, an import library's members of other DLLs.
    pub more: Vec<Shared>,
    /// Objects and archives a linker script adds (`libc_nonshared.a`): the caller reads
    /// these by name as further inputs.
    pub inputs: Vec<String>,
}

impl Shared {
    pub fn symbol_name(&self, symbol: &SharedSymbol) -> &str {
        self.names.at(symbol.name)
    }

    pub fn version(&self, symbol: &SharedSymbol) -> Option<&str> {
        symbol.version.map(|it| self.names.at(self.versions[it as usize]))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct SharedSymbol {
    pub name: Name,
    /// The version the library defines it at by default (`GLIBC_2.2.5`): an index into
    /// [`Shared::versions`].
    pub version: Option<u16>,
    pub kind: SymbolKind,
    pub weak: bool,
}

const _: () = assert!(std::mem::size_of::<SharedSymbol>() <= 16);

/// A symbol the image imports from a shared library.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: per link, not per item: a stage that gains an import, a graph rebuilt
pub(crate) struct Import {
    pub name: String,
    /// Index into [`Placed::libraries`]; `None` for a weak symbol no library defines,
    /// which the loader leaves 0 (ELF, Mach-O weak import).
    pub library: Option<u32>,
    pub version: Option<String>,
    pub kind: SymbolKind,
    pub weak: bool,
    /// wasm: the module and field it is imported from, and its type.
    pub wasm: Option<blaze_codegen::wasm::Import>,
}

/// A GOT cell and what it holds.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct GotEntry {
    pub addr: u64,
    pub holds: Holds,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Holds {
    /// The address of a symbol of the image (the core wrote it; it is in [`Placed::rebase`]).
    Local(u64),
    /// The address of an import, bound by the loader.
    Import(u32),
    /// The offset from the thread pointer of a thread-local of the image (written by the core).
    TpOff(i64),
    /// The thread pointer offset of an imported thread-local (`R_*_TPOFF64`).
    ImportTpOff(u32),
    /// A `__tls_get_addr` pair (module, offset) of a thread-local, local or imported: two cells.
    TlsGd { import: Option<u32>, offset: u64 },
    /// A TLS descriptor (two cells) of a thread-local, local or imported.
    TlsDesc { import: Option<u32>, offset: u64 },
}

/// An address holding an import's address plus `addend`, which the loader binds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Bind {
    pub addr: u64,
    pub import: u32,
    pub addend: i64,
}

/// A name the image defines for the loader and other modules: `main` of a PIE that a
/// shared library calls back, every export of a shared library, the live descriptor.
#[derive(PartialEq, Eq, Debug)]
pub(crate) struct Export {
    pub name: String,
    pub addr: u64,
    pub size: u64,
    pub kind: SymbolKind,
}

/// A placed function, for the unwind tables and the debug info.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Function {
    pub addr: u64,
    pub size: u64,
    /// Its call frame information's FDE, when it has one: in [`Class::EhFrame`], or on an ELF
    /// patch in [`Class::Patch`] after the patch's code.
    pub fde: Option<u64>,
    /// Windows: its `UNWIND_INFO` in [`Class::Rodata`], and its handler and data, resolved.
    pub windows: Option<WindowsUnwind>,
    /// A name for debuggers and backtraces: shared, since the functions list is kept across
    /// links.
    pub name: Option<Arc<str>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct WindowsUnwind {
    pub info: u64,
    pub handler: Option<u64>,
    pub lsda: Option<u64>,
}

/// A placed region.
#[derive(Clone, Debug)]
pub(crate) struct Region {
    pub class: Class,
    pub addr: u64,
    /// What its address range is: what was placed, and room to grow.
    pub capacity: u64,
    /// What was placed, from `addr`.
    pub size: u64,
    pub prot: Prot,
    pub align: u64,
}

/// Where the TLS template is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Tls {
    pub addr: u64,
    /// The initialized part's size ([`Class::Tdata`]); the rest to `size` is zeroed.
    pub init: u64,
    pub size: u64,
    pub align: u64,
}

/// An image placed and relocated, as a writer reads it. `got`, `rebase`, `binds` and
/// `functions` are in address order.
pub(crate) struct Placed<'a> {
    pub output: &'a Output,
    /// The image's file name, the last component of where it is written: a code signature's
    /// identifier, an import library's DLL.
    pub name: &'a str,
    pub plan: &'a Plan,
    /// In address order; each region's bytes are [`Placed::view`] of it.
    pub regions: &'a [Region],
    pub contents: &'a [&'a [u8]],
    /// By region, the bytes of a region held in pages rather than one slice: a code or data
    /// region whose pages a link wrote since the full link, or a writer's kept table, patched
    /// ([`Writer::synthesize`]). Its `contents` entry is empty, and it is read through
    /// [`Placed::view`]. Empty, or `None` for a region, where `contents` holds the bytes.
    pub paged: &'a [Option<Pages<'a>>],
    /// Shared libraries imported from, in the order the loader loads them.
    pub libraries: &'a [Library],
    pub imports: &'a [Import],
    pub got: &'a [GotEntry],
    /// Addresses holding an address of the image, which the loader adds the slide to.
    pub rebase: &'a [u64],
    pub binds: &'a [Bind],
    pub exports: &'a [Export],
    pub functions: &'a [Function],
    pub entry: Option<u64>,
    /// The addresses of `_init` and `_fini` when the image defines them (crti.o's `.init`
    /// and `.fini`), for ELF's `DT_INIT` and `DT_FINI`.
    pub init: Option<u64>,
    pub fini: Option<u64>,
    pub tls: Option<Tls>,
    /// What the image is: the content ID's first 16 bytes, as its build ID or `LC_UUID`.
    pub id: [u8; 16],
    /// The debug sections the core made ([`crate::dwarf`]), by DWARF name (`.debug_line`).
    pub debug: &'a [(&'static str, Vec<u8>)],
    /// The address of a global name the image defines, an object's or a special (PE's data
    /// directories point at the CRT's `_tls_used` and `_load_config_used`).
    // dyn: a handful of calls per image (PE's data directories), never per element
    pub symbol: &'a (dyn Fn(&str) -> Option<u64> + Sync),
    /// This link's synthesis: a number no other synthesis in the process has. A writer that
    /// keeps its last tables records it, and patches them only for a [`Delta::from`] equal to
    /// it (a rollback brings back a state its last tables are not of).
    pub synthesis: u64,
    /// What changed since the last link's synthesis, when the core knows it: `None` on a full
    /// link and on an image's first.
    pub delta: Option<&'a Delta<'a>>,
    /// The slots [`Writer::encode`] gave, (address, value) in address order: what the file
    /// holds there in place of the placed bytes. Empty for [`Writer::synthesize`] and
    /// [`Writer::encode`] itself; set for [`Writer::file`].
    pub encoded: &'a [(u64, u64)],
}

/// One run of a `Placed` list replaced since the last link: `old` in the last link's list,
/// `new` in this one's.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Splice {
    pub old: Range<u32>,
    pub new: Range<u32>,
}

impl Splice {
    /// Whether it replaces as many entries as it removes, each in its place.
    #[inline]
    pub fn same_len(&self) -> bool {
        self.old.len() == self.new.len()
    }
}

/// What changed since the last `synthesize` of this image, when the core knows it. Each list's
/// splices are in order and disjoint; a run whose entries are all equal is not one.
pub(crate) struct Delta<'a> {
    pub functions: &'a [Splice],
    pub rebase: &'a [Splice],
    pub got: &'a [Splice],
    pub binds: &'a [Splice],
    /// `imports`, `exports` and `libraries` are the last link's (the same `Arc`s, and exports
    /// of the same names, addresses, sizes and kinds).
    pub same_names: bool,
    /// No region moved or changed size, but for the writer's own synthetic regions, whose
    /// size it makes.
    pub same_regions: bool,
    /// Address ranges of the regions the core wrote since the last link, by address, none
    /// overlapping. In the [`Placed`] that [`Writer::file`] sees, they include the blocks of
    /// the synthetic regions that differ from the last link's.
    pub written: &'a [(u64, u64)],
    /// The [`Placed::synthesis`] of the last link, which these changes are from.
    pub from: u64,
}

impl Delta<'_> {
    /// Whether a writer's last tables, made by synthesis `last`, may be patched with this:
    /// they are of the link this is from, no region moved and every splice of `lists` keeps
    /// its length.
    pub fn patches(&self, last: u64, lists: &[&[Splice]]) -> bool {
        self.from == last && self.same_regions && lists.iter().all(|it| it.iter().all(Splice::same_len))
    }
}

impl<'a> Placed<'a> {
    /// The bytes placed in `region` (index into [`Placed::regions`]) when they are one slice:
    /// a synthetic region a writer made whole. Empty for zeroed ones. Code and data regions,
    /// and kept tables, may be in pages ([`Placed::paged`]): reading one here is a writer's
    /// bug, and it reads them through [`Placed::view`].
    pub fn bytes(&self, region: usize) -> &[u8] {
        assert!(self.paged(region).is_none(), "region {region}'s bytes are in pages: read them with `Placed::view`");
        self.contents[region]
    }

    /// The bytes of `region` when they are held in pages ([`Placed::paged`]).
    #[inline]
    pub fn paged(&self, region: usize) -> Option<Pages<'a>> {
        self.paged.get(region).copied().flatten()
    }

    /// `region`'s bytes, one slice or in pages.
    #[inline]
    pub fn view(&self, region: usize) -> View<'a> {
        View::of(self.contents, self.paged, region)
    }

    /// How many bytes are placed in `region`, paged or not.
    pub fn len(&self, region: usize) -> usize {
        self.view(region).len()
    }

    /// Each run of `region`'s bytes as (offset in the region, bytes), in order ([`View::runs`]).
    pub fn runs(&self, region: usize, f: impl FnMut(usize, &'a [u8])) {
        self.view(region).runs(f);
    }

    /// The 8 bytes at `addr` of the region holding them, little-endian: none outside every
    /// region's placed bytes.
    pub fn u64_at(&self, addr: u64) -> Option<u64> {
        let index = self.regions.partition_point(|it| it.addr <= addr).checked_sub(1)?;
        let at = usize::try_from(addr - self.regions[index].addr).ok()?;
        self.view(index).u64(at)
    }

    pub fn region(&self, class: &Class) -> Option<(usize, &Region)> {
        self.regions.iter().enumerate().find(|(_, it)| &it.class == class)
    }
}

/// A region's bytes held in pages ([`Placed::paged`]): the first `len` of `bytes`, which may
/// hold more (a region that shrank keeps its pages).
#[derive(Clone, Copy)]
pub(crate) struct Pages<'a> {
    pub bytes: &'a CowBytes,
    pub len: usize,
}

impl<'a> Pages<'a> {
    pub fn all(bytes: &'a CowBytes) -> Pages<'a> {
        Pages { bytes, len: bytes.len() }
    }
}

/// A region's bytes as a writer reads them ([`Placed::view`]): one slice, or pages, which are
/// read run by run, a word at a time, or a record at a time through a reused buffer. Nothing
/// is copied for a read that one run holds.
#[derive(Clone, Copy)]
pub(crate) enum View<'a> {
    Slice(&'a [u8]),
    Pages(Pages<'a>),
}

impl<'a> View<'a> {
    /// Region `region`'s view of `contents` and `paged` ([`Placed::contents`], [`Placed::paged`]),
    /// for what reads them where `Placed` can't go (another thread).
    #[inline]
    pub fn of(contents: &'a [&'a [u8]], paged: &'a [Option<Pages<'a>>], region: usize) -> View<'a> {
        match paged.get(region).copied().flatten() {
            Some(pages) => View::Pages(pages),
            None => View::Slice(contents[region]),
        }
    }

    #[inline]
    pub fn len(self) -> usize {
        match self {
            View::Slice(bytes) => bytes.len(),
            View::Pages(pages) => pages.len,
        }
    }

    #[inline]
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// `at..at + len` borrowed, when it is within the bytes and one run holds it.
    #[inline]
    pub fn slice(self, at: usize, len: usize) -> Option<&'a [u8]> {
        if at.checked_add(len)? > self.len() {
            return None;
        }
        match self {
            View::Slice(bytes) => Some(&bytes[at..at + len]),
            View::Pages(pages) => pages.bytes.slice(at, len),
        }
    }

    /// `self[at..at + out.len()]` into `out`; none of it past the end, which is a bug.
    pub fn copy_to(self, at: usize, out: &mut [u8]) {
        assert!(at + out.len() <= self.len(), "a read past the region's bytes");
        match self {
            View::Slice(bytes) => out.copy_from_slice(&bytes[at..at + out.len()]),
            View::Pages(pages) => pages.bytes.copy_to(at, out),
        }
    }

    /// `at..at + len`: borrowed when one run holds it, else copied into `scratch`, which
    /// grows only to the longest such read and is reused across them. None past the end.
    pub fn read<'s>(self, at: usize, len: usize, scratch: &'s mut Vec<u8>) -> Option<&'s [u8]>
    where
        'a: 's,
    {
        if at.checked_add(len)? > self.len() {
            return None;
        }
        if let Some(bytes) = self.slice(at, len) {
            return Some(bytes);
        }
        scratch.clear();
        // alloc: none once `scratch` holds the longest read across pages (a record or a function)
        scratch.resize(len, 0);
        self.copy_to(at, scratch);
        Some(&scratch[..])
    }

    /// The `N` bytes at `at`, across a page boundary or not; none past the end.
    #[inline]
    pub fn array<const N: usize>(self, at: usize) -> Option<[u8; N]> {
        if let Some(bytes) = self.slice(at, N) {
            return bytes.try_into().ok();
        }
        if at.checked_add(N)? > self.len() {
            return None;
        }
        let mut out = [0; N];
        self.copy_to(at, &mut out);
        Some(out)
    }

    #[inline]
    pub fn u32(self, at: usize) -> Option<u32> {
        self.array(at).map(u32::from_le_bytes)
    }

    #[inline]
    pub fn u64(self, at: usize) -> Option<u64> {
        self.array(at).map(u64::from_le_bytes)
    }

    /// Each run of the bytes as (offset, bytes), in order, none empty: the slice whole, or the
    /// runs of the pages up to `len` ([`CowBytes::runs`]). Bytes in no run are zeros.
    pub fn runs(self, mut f: impl FnMut(usize, &'a [u8])) {
        match self {
            View::Slice(bytes) if bytes.is_empty() => {}
            View::Slice(bytes) => f(0, bytes),
            View::Pages(Pages { bytes, len }) => bytes.runs(|at, run| {
                if at < len {
                    f(at, &run[..run.len().min(len - at)]);
                }
            }),
        }
    }

    /// The first `len` bytes; none past the end.
    pub fn prefix(self, len: usize) -> Option<View<'a>> {
        (len <= self.len()).then(|| match self {
            View::Slice(bytes) => View::Slice(&bytes[..len]),
            View::Pages(pages) => View::Pages(Pages { bytes: pages.bytes, len }),
        })
    }

    /// The `N`-byte chunks of `self[at..end]` in order, by value: read from a slice borrowed a
    /// page at a time, and a chunk across two pages put together. `end - at` is a multiple of
    /// `N`, within the bytes.
    pub fn arrays<const N: usize>(self, at: usize, end: usize) -> Arrays<'a, N> {
        assert!(at <= end && end <= self.len() && (end - at).is_multiple_of(N), "{N}-byte chunks of {at}..{end}");
        let window = match self {
            View::Slice(bytes) => &bytes[at..end],
            View::Pages(_) => &[][..],
        };
        Arrays { view: self, at, end, window, window_at: at }
    }
}

/// [`View::arrays`].
pub(crate) struct Arrays<'a, const N: usize> {
    view: View<'a>,
    at: usize,
    end: usize,
    /// Borrowed bytes from `window_at`: the slice whole, or the rest of a page.
    window: &'a [u8],
    window_at: usize,
}

impl<const N: usize> Iterator for Arrays<'_, N> {
    type Item = [u8; N];

    #[inline]
    fn next(&mut self) -> Option<[u8; N]> {
        if self.at >= self.end {
            return None;
        }
        let at = self.at;
        self.at += N;
        if at >= self.window_at && at + N <= self.window_at + self.window.len() {
            let from = at - self.window_at;
            return self.window[from..from + N].try_into().ok();
        }
        const PAGE: usize = CowBytes::PAGE;
        let stop = ((at / PAGE + 1) * PAGE).min(self.end);
        if let Some(window) = self.view.slice(at, stop - at) {
            (self.window, self.window_at) = (window, at);
            if at + N <= stop {
                return window[..N].try_into().ok();
            }
        }
        self.view.array(at)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = (self.end - self.at.min(self.end)) / N;
        (left, Some(left))
    }
}

/// A shared library the image needs.
#[derive(PartialEq, Eq, Debug)]
pub(crate) struct Library {
    pub name: String,
    /// Loaded only for a symbol it gives (`--as-needed`): one none of whose symbols are
    /// imported is left out.
    pub as_needed: bool,
}

/// The counts a writer sizes its synthetic regions by, known before anything is placed.
#[derive(Clone, Default, Debug)]
pub(crate) struct Counts {
    pub imports: u64,
    /// The total length of the imports' names and versions.
    pub import_names: u64,
    pub libraries: u64,
    pub library_names: u64,
    pub got: u64,
    /// Places the loader slides or binds: every [`Apply::Abs64`] and every GOT cell.
    pub absolute: u64,
    pub exports: u64,
    pub export_names: u64,
    pub functions: u64,
    /// The total length of the functions' debug names.
    pub function_names: u64,
    /// Bytes of code and data: a code signature hashes every page of them.
    pub image: u64,
    /// The FDEs that will be in [`Class::EhFrame`]: Rust's and the native objects'.
    pub fdes: u64,
    /// The bytes of the kept native sections of [`SectionKind::Unwind`].
    pub unwind: u64,
}

/// The file a writer makes of a placed image.
pub(crate) struct File {
    pub len: u64,
    /// Byte ranges of the file and what is in them: the headers, the synthetic regions'
    /// tables outside the address space (symbol tables, a signature). Regions themselves
    /// are written by the core at [`Writer::offset`]. A table the writer keeps across links
    /// is a clone of what it keeps, so a patch shares every page it didn't write.
    pub parts: Vec<(u64, CowBytes)>,
    /// The debug sections in the file, outside the address space: (file offset, index into
    /// [`Placed::debug`]). The core lays them from the sections it made, so no writer copies
    /// them, and compares them by block with the last link's as it does parts.
    pub debug: Vec<(u64, usize)>,
}

/// An object format.
pub(crate) trait Writer: Send + Sync {
    fn format(&self) -> Format;

    fn arch(&self) -> Arch;

    fn plan(&self) -> Plan;

    /// Sorts the classes of the image into the order of the address space, each region a
    /// run of one protection ([`Writer::prot`]).
    fn order(&self, classes: &mut Vec<Class>);

    fn prot(&self, class: &Class) -> Prot;

    /// The class a section a static or native object names goes in, and its rank in that
    /// class: pieces of a lower rank go first, of one rank in input order. `.init_array.00099`
    /// is [`Class::InitArray`] of rank 99 (before `.init_array`, of rank `u32::MAX`, as GNU
    /// ld sorts them), `.CRT$XCU` the PE writer's own; `linkme_ITEMS` a [`Class::Named`].
    fn section(&self, name: &str, write: bool, exec: bool, tls: bool)
    -> Result<(Class, u32), String>;

    /// The symbol the linker defines under `name`, if it defines one.
    fn special(&self, name: &str) -> Option<Special>;

    /// The class and rank of a dev run's pointer to `blaze_inproc_start` ([`crate::DevRun`]),
    /// which the core places before every other piece of that class and rank; an error for a
    /// format with no start hook.
    fn start_hook(&self) -> Result<(Class, u32), String>;

    /// The entry of a shared image, rooted and resolved as an executable's (PE's
    /// `_DllMainCRTStartup`, which runs a DLL's CRT and C++ initializers).
    fn shared_entry(&self) -> Option<&'static str> {
        None
    }

    /// The class and rank of a piece the core places with no section name of its own
    /// (codegen's code and statics), given the class the core would use.
    fn own(&self, class: &Class) -> (Class, u32) {
        // clone: once per class per stage (the stage caches what it gives)
        (class.clone(), u32::MAX)
    }

    /// Reads a native input of the format.
    fn read(&self, name: &str, bytes: &[u8], kind: &crate::NativeKind)
    -> Result<NativeFile, String>;

    /// The synthetic regions the writer fills and how big each may get.
    fn synthetic(&self, counts: &Counts) -> Vec<(Class, u64)>;

    /// Applies an [`Apply::Native`] relocation at `place` of `bytes`, which is at address
    /// `p`, to the target at `s`.
    fn apply(&self, r_type: u32, bytes: &mut [u8], p: u64, s: u64, addend: i64)
    -> Result<(), String>;

    /// The contents of each synthetic region, the image placed and its other regions filled. A
    /// table the writer keeps across links is a clone of what it keeps, so a patch shares
    /// every page it didn't write.
    fn synthesize(&self, placed: &Placed<'_>) -> Result<Vec<(Class, CowBytes)>, LinkError>;

    /// Where the region at `addr` is in the file: `None` for one not in the file (zeroed).
    fn offset(&self, placed: &Placed<'_>, region: &Region) -> Option<u64>;

    /// The file's headers and its parts outside the address space. `placed` holds each
    /// synthetic region's bytes as [`Writer::synthesize`] returned them.
    fn file(&self, placed: &Placed<'_>) -> Result<File, LinkError>;

    /// The 8-byte slots the file holds other than as placed, as (address, value) in address
    /// order: Mach-O's chained fixups, where each rebase and bind slot holds its target and
    /// the distance to the next. Called with what [`Writer::file`] sees. The regions keep the
    /// plain values; the core lays these over them in the file and, on an incremental link,
    /// writes only the slots whose value changed. None by default.
    fn encode(&self, placed: &Placed<'_>) -> Result<Vec<(u64, u64)>, LinkError> {
        let _ = placed;
        // alloc: none: an empty `Vec` holds no buffer
        Ok(Vec::new())
    }
}

/// The writer of each format, dispatched by its tag: a `match` per call, inlined into each
/// format's method, never a virtual call.
pub(crate) enum Formats {
    Elf(crate::elf::Elf),
    MachO(crate::macho::MachO),
    Pe(crate::pe::Pe),
    Wasm(crate::wasm::Wasm),
}

macro_rules! each {
    ($self:ident, $it:ident => $call:expr) => {
        match $self {
            Formats::Elf($it) => $call,
            Formats::MachO($it) => $call,
            Formats::Pe($it) => $call,
            Formats::Wasm($it) => $call,
        }
    };
}

impl Writer for Formats {
    #[inline]
    fn format(&self) -> Format {
        each!(self, it => it.format())
    }

    #[inline]
    fn arch(&self) -> Arch {
        each!(self, it => it.arch())
    }

    fn plan(&self) -> Plan {
        each!(self, it => it.plan())
    }

    fn order(&self, classes: &mut Vec<Class>) {
        each!(self, it => it.order(classes))
    }

    #[inline]
    fn prot(&self, class: &Class) -> Prot {
        each!(self, it => it.prot(class))
    }

    #[inline]
    fn section(&self, name: &str, write: bool, exec: bool, tls: bool) -> Result<(Class, u32), String> {
        each!(self, it => it.section(name, write, exec, tls))
    }

    #[inline]
    fn special(&self, name: &str) -> Option<Special> {
        each!(self, it => it.special(name))
    }

    fn shared_entry(&self) -> Option<&'static str> {
        each!(self, it => it.shared_entry())
    }

    fn start_hook(&self) -> Result<(Class, u32), String> {
        each!(self, it => it.start_hook())
    }

    #[inline]
    fn own(&self, class: &Class) -> (Class, u32) {
        each!(self, it => it.own(class))
    }

    fn read(&self, name: &str, bytes: &[u8], kind: &crate::NativeKind) -> Result<NativeFile, String> {
        each!(self, it => it.read(name, bytes, kind))
    }

    fn synthetic(&self, counts: &Counts) -> Vec<(Class, u64)> {
        each!(self, it => it.synthetic(counts))
    }

    #[inline]
    fn apply(&self, r_type: u32, bytes: &mut [u8], p: u64, s: u64, addend: i64) -> Result<(), String> {
        each!(self, it => it.apply(r_type, bytes, p, s, addend))
    }

    fn synthesize(&self, placed: &Placed<'_>) -> Result<Vec<(Class, CowBytes)>, LinkError> {
        each!(self, it => it.synthesize(placed))
    }

    fn offset(&self, placed: &Placed<'_>, region: &Region) -> Option<u64> {
        each!(self, it => it.offset(placed, region))
    }

    fn file(&self, placed: &Placed<'_>) -> Result<File, LinkError> {
        each!(self, it => it.file(placed))
    }

    fn encode(&self, placed: &Placed<'_>) -> Result<Vec<(u64, u64)>, LinkError> {
        each!(self, it => it.encode(placed))
    }
}

/// The writer of an output's format.
pub(crate) fn writer(output: &Output) -> Result<Formats, LinkError> {
    if output.exec_log {
        exec_log(output).map_err(|what| LinkError { function: None, what: what.into(), refusal: None })?;
    }
    Ok(match output.format {
        Format::Elf => Formats::Elf(crate::elf::writer(output)?),
        Format::MachO => Formats::MachO(crate::macho::writer(output)?),
        Format::Pe => Formats::Pe(crate::pe::writer(output)?),
        Format::Wasm => Formats::Wasm(crate::wasm::writer(output)?),
    })
}

/// Why `output` can't have the jump table's execution log (fastcompile-ident-select 9), if it
/// can't: the log stubs sit beside the slot stubs, and the runtime finds the log through the
/// live descriptor's table.
fn exec_log(output: &Output) -> Result<(), &'static str> {
    if output.format == Format::Wasm {
        return Err("an execution log on wasm, which reaches code by index and has no slot stubs to log calls in");
    }
    if !output.incremental {
        return Err("an execution log in an image linked without a jump table (not incremental)");
    }
    if output.live.is_none() {
        return Err("an execution log in an image with no live descriptor, by which the runtime finds the log");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_execution_log_is_refused_where_there_are_no_slot_stubs() {
        let ok = Output {
            format: Format::Elf,
            arch: Arch::X86_64,
            kind: crate::OutputKind::Executable { entry: "_start".into() },
            interpreter: Some("/lib64/ld-linux-x86-64.so.2".into()),
            platform: None,
            incremental: true,
            debug: crate::Debug::None,
            live: Some(crate::Live),
            tags: 1 << 32,
            stack: None,
            windows_subsystem: false,
            dev_run: None,
            triple: "x86_64-unknown-linux-gnu".into(),
            exec_log: true,
            sanitize: blaze_codegen::settings::Sanitize::default(),
            rpaths: Vec::new(),
        };
        assert!(exec_log(&ok).is_ok());
        assert!(writer(&ok).is_ok());
        for (format, incremental, live, needs) in [
            (Format::Wasm, true, true, "wasm"),
            (Format::Elf, false, true, "without a jump table"),
            (Format::Elf, true, false, "no live descriptor"),
        ] {
            let mut output = ok.clone();
            output.format = format;
            output.incremental = incremental;
            if !live {
                output.live = None;
            }
            let why = exec_log(&output).expect_err("refused");
            assert!(why.contains(needs), "{why}");
        }
    }

    /// A region in pages, some written since its buffer and its end past the buffer, reads
    /// as the same bytes in one slice do: words and records across page boundaries, chunks,
    /// runs and a prefix.
    #[test]
    fn a_view_in_pages_reads_as_one_slice() {
        let page = CowBytes::PAGE;
        let mut flat: Vec<u8> = (0..3 * page + 100).map(|it| (it * 7 + 3) as u8).collect();
        let mut paged = CowBytes::from_vec(flat[..3 * page].to_vec());
        paged.resize(flat.len());
        flat[3 * page..].fill(0);
        for at in [page - 3, 2 * page + 9] {
            paged.write(at, &flat[at..at + 6]);
        }
        let len = flat.len() - 40;
        let views = [View::Slice(&flat[..len]), View::Pages(Pages { bytes: &paged, len })];
        let [slice, pages] = views;
        assert!(pages.slice(page - 3, 6).is_none(), "a word across a page boundary is not one run");
        let mut scratch = Vec::new();
        for at in [0, page - 8, page - 3, page - 1, 2 * page + 9, 3 * page - 2, len - 8, len - 7] {
            assert_eq!(slice.u64(at), pages.u64(at), "u64 at {at}");
            assert_eq!(slice.u32(at), pages.u32(at), "u32 at {at}");
            let want = slice.read(at, 7.min(len - at), &mut Vec::new()).map(<[u8]>::to_vec);
            assert_eq!(pages.read(at, 7.min(len - at), &mut scratch).map(<[u8]>::to_vec), want, "read at {at}");
        }
        assert_eq!((slice.u64(len - 7), pages.read(len - 3, 4, &mut scratch)), (None, None));
        for (at, end) in [(0, len - len % 12), (4, 4 + 12 * 2000), (page - 6, page + 6)] {
            let want: Vec<[u8; 12]> = slice.arrays(at, end).collect();
            assert_eq!(pages.arrays::<12>(at, end).collect::<Vec<_>>(), want, "chunks of {at}..{end}");
        }
        let mut rebuilt = vec![0u8; len];
        pages.runs(|at, run| rebuilt[at..at + run.len()].copy_from_slice(run));
        assert_eq!(rebuilt, flat[..len]);
        let short = pages.prefix(page + 1).unwrap();
        assert_eq!((short.len(), short.u32(page - 3), short.u32(page - 2)), (page + 1, slice.u32(page - 3), None));
        assert!(pages.prefix(len + 1).is_none());
    }

    #[test]
    fn a_suffix_names_the_bytes_inside_a_name() {
        let mut names = Names::default();
        names.push("x").unwrap();
        let name = names.push("_main€").unwrap();
        assert_eq!(names.at(names.suffix(name, 0).unwrap()), "_main€");
        assert_eq!(names.at(names.suffix(name, 1).unwrap()), "main€");
        assert_eq!(names.at(names.suffix(name, 8).unwrap()), "");
        assert_eq!(names.suffix(name, 9), None);
        assert_eq!(names.suffix(name, 6), None);
        assert_eq!(names.bytes(), 1 + "_main€".len());
    }

    /// A name looked up, whole or as a suffix, allocates nothing, and neither does a push into
    /// room reserved for it.
    #[test]
    fn a_names_lookup_allocates_nothing() {
        let mut names = Names::with_capacity(16 * 1000);
        let list: Vec<Name> = (0..1000).map(|n| names.push(&format!("symbol{n}")).unwrap()).collect();
        blaze_alloc_count::assert_at_most(0, "a name's bytes and a suffix of it", || {
            assert_eq!(names.at(list[500]), "symbol500");
            assert_eq!(names.get(list[999]), Some("symbol999"));
            assert_eq!(names.at(names.suffix(list[7], 6).unwrap()), "7");
        });
        let pushed = blaze_alloc_count::assert_at_most(0, "a push into reserved room", || names.push("one more"));
        assert_eq!(names.at(pushed.unwrap()), "one more");
    }
}
