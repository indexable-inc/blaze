//! The split: one relocatable object (one translation unit's, as the pinned clang writes it) cut
//! into ATOMS, one a function and one a data object, each with the relocations inside it turned
//! into references to OTHER SYMBOLS BY NAME (c.md part 3: "Its output: code records, one a
//! function").
//!
//! The object is read by blaze-native's readers (the reader's one home, rule 30), then:
//!
//! **Atoms.** An atom is a run of one section's bytes that nothing outside it reaches into but
//! by a symbol or a relocation; which runs those are is the format's own rule:
//!
//! - Mach-O: the header's `MH_SUBSECTIONS_VIA_SYMBOLS` flag, which clang always sets, says "the
//!   sections of the object file can be divided into individual blocks" at each symbol
//!   (`<mach-o/loader.h>`): an atom runs from a symbol to the next symbol of its section, or to
//!   the section's end. An object without the flag is refused ([`SplitError::NotSubsections`]).
//! - ELF: no such promise exists (the assembler resolves a branch inside one section with no
//!   relocation), so a section is ONE atom, and the driver compiles with `-ffunction-sections
//!   -fdata-sections`. A section whose symbols stand at two offsets is refused by name
//!   ([`SplitError::SharedSection`]), never cut.
//!
//! Bytes before a section's first symbol, and a section with no symbol (a mergeable string
//! section), are an ANONYMOUS atom ([`AtomName::Anonymous`]). Symbols at one offset are one atom:
//! its name is the first global among them in symbol-table order (else the first local), the
//! rest are its aliases. ELF mapping symbols (`$x`, `$d`), section and file symbols are not names.
//!
//! **Names.** A global's name is its C name: Mach-O's leading `_` is taken off, so `_foo` is
//! `foo` on both formats and `Symbol::of_name` of it is the symbol Rust's records already name
//! it by. A local's name is unit-local: its text and how many atoms before it bear the same text
//! ([`AtomName::Local`]); its symbol is made from the unit key ([`crate::key::local_symbol`]).
//!
//! **References.** Each relocation lies wholly inside one atom ([`SplitError::RefAcross`]) and
//! becomes a [`Ref`] at its offset in that atom: against a GLOBAL symbol (defined here or not),
//! by its name ([`RefTo::Name`]), so the symbol index decides which unit's definition it reaches
//! (a weak definition here may lose to a strong one elsewhere); against a LOCAL symbol or a
//! section, by the atom it lands in ([`RefTo::Atom`]), the addend made relative to that atom's
//! first byte.
//!
//! **What is not an atom.** `__mod_init_func`/`.init_array` (and the term/fini forms) become the
//! unit's [`Init`] rows, each one 8-byte pointer to a function, with its priority (ELF's
//! `.init_array.N` suffix; 65536, after every numbered one, for none). Mach-O's
//! `__LD,__compact_unwind` rows become their atoms' [`Unwind`]. `.eh_frame`/`__eh_frame` is not
//! read: an ELF atom carries no unwind row, and the lowering states what that means
//! (lower.rs, "Unwind"). Debug sections are blaze-native's `Skip` and are not read. A section of a
//! kind none of these covers is refused by name ([`SplitError::Section`]).
//!
//! Deterministic: atoms in (section, offset) order, references in offset order, every list a
//! function of the bytes alone (rule 25).

use core::fmt;
use core::ops::Range;

use blaze_native::Platform;
use blaze_native::object::{self, Bind, Def, Object, ReadError, RelocKind, RelocTo, SectionKind};
use blaze_own::planted::{Planted, Plants};
use blaze_own::{AVec, Alloc, Vec};

/// The bugs a control can plant in the split (D165).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SplitPlant {
    /// The split as it is.
    None,
    /// The first relocation of each section is dropped: an atom loses a reference.
    DropsRef,
}

impl Plants for SplitPlant {}

/// `MH_SUBSECTIONS_VIA_SYMBOLS` (`<mach-o/loader.h>`).
const MH_SUBSECTIONS_VIA_SYMBOLS: u32 = 0x2000;

/// The priority of an init row with none: after every numbered one (lld's order for a plain
/// `.init_array`).
pub const NO_PRIORITY: u32 = 65_536;

/// What an atom holds, from its section's kind.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AtomKind {
    Code,
    ReadOnly,
    CStrings,
    Data,
    /// Zero-filled: no bytes, `size` of them.
    Zero,
    ThreadData,
    ThreadZero,
    /// Mach-O's thread-local descriptors (`__thread_vars`).
    ThreadVars,
    /// A C++ exception table (`__gcc_except_tab`, `.gcc_except_table`).
    ExceptTable,
}

impl AtomKind {
    fn of(kind: SectionKind) -> Option<AtomKind> {
        Some(match kind {
            SectionKind::Code => AtomKind::Code,
            SectionKind::ReadOnly => AtomKind::ReadOnly,
            SectionKind::CStrings => AtomKind::CStrings,
            SectionKind::Data => AtomKind::Data,
            SectionKind::ZeroFill => AtomKind::Zero,
            SectionKind::ThreadData => AtomKind::ThreadData,
            SectionKind::ThreadZero => AtomKind::ThreadZero,
            SectionKind::ThreadVars => AtomKind::ThreadVars,
            SectionKind::ExceptTable => AtomKind::ExceptTable,
            SectionKind::InitPointers
            | SectionKind::TermPointers
            | SectionKind::EhFrame
            | SectionKind::CompactUnwind
            | SectionKind::Skip
            | SectionKind::Other => return None,
        })
    }

    /// Whether the atom is a function's code.
    pub fn is_code(self) -> bool {
        self == AtomKind::Code
    }
}

/// How an atom's name binds across units. The symbol index chooses among definitions by it
/// (index.rs).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Strength {
    /// Seen by this unit alone.
    Local,
    /// One definition in the program: a second is a refusal.
    Strong,
    /// Mach-O `N_WEAK_DEF`, ELF `STB_WEAK`: a strong definition wins over it.
    Weak,
    /// In an ELF COMDAT group: any one definition stands for all.
    Comdat,
}

/// An atom's name: what its record's key and its symbol are made from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AtomName<'a> {
    /// A C name every unit sees (Mach-O's `_` taken off).
    Global(&'a [u8]),
    /// A name of this unit alone, and how many atoms before it bear the same text.
    Local { name: &'a [u8], nth: u32 },
    /// No symbol stands at the atom's first byte: its section (the file's index) and offset.
    Anonymous { section: u32, start: u64 },
}

/// What a reference reaches.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RefTo<'a> {
    /// A global C name; `weak` for a weak undefined reference (the address is 0 when nothing
    /// defines it).
    Name { name: &'a [u8], weak: bool },
    /// Atom `n` of this split.
    Atom(u32),
}

/// A relocation's computation, as blaze-native reads it, with the subtracted symbol of a
/// difference moved to [`Ref::minus`] (so no index of the file is left in a [`Ref`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RefKind {
    Abs64,
    Abs32,
    Rel32,
    Rel64,
    Delta32,
    Delta64,
    Branch26,
    CondBranch19,
    TestBranch14,
    Page21,
    PageOff12 { scale_log2: u8 },
    GotPage21,
    GotPageOff12,
    GotRel32,
    TlvPage21,
    TlvPageOff12,
    TlsDescPage21,
    TlsDescLo12,
    TlsDescAdd,
    TlsDescCall,
    TlsIePage21,
    TlsIeLo12,
    TlsLeHi12,
    TlsLeLo12,
}

/// One reference: at `at` (from the atom's first byte), write `kind` of `to` plus `addend`, less
/// `minus` (an atom or a name, plus its offset) for a difference. For [`RefTo::Atom`], `addend`
/// counts from that atom's first byte.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ref<'a> {
    pub at: u32,
    pub kind: RefKind,
    pub to: RefTo<'a>,
    pub addend: i64,
    pub minus: Option<(RefTo<'a>, i64)>,
}

/// An atom's unwind row (Mach-O `__compact_unwind`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Unwind<'a> {
    pub length: u32,
    /// The compact unwind encoding, as the row has it.
    pub encoding: u32,
    pub personality: Option<RefTo<'a>>,
    /// The LSDA: an atom and the offset in it.
    pub lsda: Option<(u32, u64)>,
}

/// One function or data object of the unit.
#[derive(Clone, Debug)]
pub struct Atom<'a> {
    pub name: AtomName<'a>,
    pub kind: AtomKind,
    pub strength: Strength,
    /// Mach-O `N_PEXT`, ELF hidden or internal: still one definition across the unit's program.
    pub hidden: bool,
    /// The section (the file's index) and the first byte's offset in it.
    pub section: u32,
    pub start: u64,
    pub size: u64,
    pub align_log2: u8,
    /// Empty for a zero-filled atom.
    pub bytes: &'a [u8],
    /// Its run of [`Split::refs`].
    pub refs: Range<u32>,
    /// Its run of [`Split::aliases`]: the other global names at its first byte.
    pub aliases: Range<u32>,
    pub unwind: Option<Unwind<'a>>,
}

/// One initializer or finalizer of the unit: a function to call before `main` (after, for a
/// finalizer), in (priority, the unit's order, this list's order).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Init<'a> {
    pub fini: bool,
    pub priority: u32,
    pub to: RefTo<'a>,
    pub addend: i64,
}

/// One unit's object, split.
pub struct Split<'a> {
    pub atoms: AVec<'a, Atom<'a>>,
    pub refs: AVec<'a, Ref<'a>>,
    pub aliases: AVec<'a, &'a [u8]>,
    pub inits: AVec<'a, Init<'a>>,
}

impl<'a> Split<'a> {
    /// Atom `n`'s references.
    pub fn refs_of(&self, n: usize) -> &[Ref<'a>] {
        let r = &self.atoms[n].refs;
        &self.refs[r.start as usize..r.end as usize]
    }

    /// Atom `n`'s global aliases.
    pub fn aliases_of(&self, n: usize) -> &[&'a [u8]] {
        let r = &self.atoms[n].aliases;
        &self.aliases[r.start as usize..r.end as usize]
    }
}

/// What [`split`] is told.
#[derive(Clone, Copy)]
pub struct SplitOpts<'a> {
    pub platform: Platform,
    pub alloc: Alloc<'a>,
}

/// Bytes of a name or section, shown as text where they are UTF-8, else as hex.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Shown<'a>(pub &'a [u8]);

impl fmt::Display for Shown<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match core::str::from_utf8(self.0) {
            Ok(text) => f.write_str(text),
            Err(_) => self.0.iter().try_for_each(|byte| write!(f, "\\x{byte:02x}")),
        }
    }
}

impl fmt::Debug for Shown<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "\"{self}\"")
    }
}

/// Why an object was not split: each case names the section (by its name) and the offset in it,
/// or the symbol.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SplitError<'a> {
    /// blaze-native refused the bytes (its `at` is an offset in the file).
    Read(ReadError),
    /// A Mach-O object without `MH_SUBSECTIONS_VIA_SYMBOLS`: its sections cannot be cut at
    /// symbols.
    NotSubsections,
    /// An ELF section whose symbols stand at two offsets (`at` is the second): one function or
    /// object a section is what `-ffunction-sections -fdata-sections` makes.
    SharedSection { section: Shown<'a>, at: u64 },
    /// A section of a kind no atom, init row or unwind row is made of.
    Section { section: Shown<'a>, kind: SectionKind },
    /// A tentative (common) definition: the driver compiles with `-fno-common`.
    Common { symbol: Shown<'a> },
    /// A global absolute symbol: a value, not a place, which no record holds.
    Absolute { symbol: Shown<'a> },
    /// A Mach-O global whose name does not start with `_`: it has no C name.
    NoCName { symbol: Shown<'a> },
    /// A symbol past its section's end.
    SymbolOutside { symbol: Shown<'a>, section: Shown<'a>, offset: u64 },
    /// A relocation whose field is not inside one atom.
    RefAcross { section: Shown<'a>, offset: u64 },
    /// A section-relative relocation's target offset is past its section.
    RefTarget { section: Shown<'a>, offset: u64 },
    /// A relocation of a section no atom is made of (offset in that section).
    RefInNoAtom { section: Shown<'a>, offset: u64 },
    /// An init or term section whose entry at `offset` is not one 8-byte pointer relocation.
    InitEntry { section: Shown<'a>, offset: u64 },
    /// An init section's suffix is not a priority (`.init_array.N`).
    InitPriority { section: Shown<'a> },
    /// A compact unwind row whose function is not an atom's first byte.
    UnwindNotAtAtom { section: Shown<'a>, offset: u64 },
    /// Two unwind rows for one atom.
    UnwindTwice { section: Shown<'a>, offset: u64 },
}

impl fmt::Display for SplitError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let at = "(blaze-cunit split.rs; c.md part 3)";
        match *self {
            SplitError::Read(why) => write!(f, "the unit's object does not read: {why:?} {at}"),
            SplitError::NotSubsections => write!(f, "the unit's Mach-O object lacks MH_SUBSECTIONS_VIA_SYMBOLS, so it cannot be cut at its symbols {at}"),
            SplitError::SharedSection { section, at: offset } => write!(f, "the unit's ELF section {section} holds a second symbol at {offset}: one function or object a section is needed (-ffunction-sections -fdata-sections) {at}"),
            SplitError::Section { section, kind } => write!(f, "the unit's section {section} is of kind {kind:?}, of which no record is made {at}"),
            SplitError::Common { symbol } => write!(f, "the unit's symbol {symbol} is a tentative (common) definition: the unit is compiled with -fno-common {at}"),
            SplitError::Absolute { symbol } => write!(f, "the unit's global symbol {symbol} is absolute: a value, which no record holds {at}"),
            SplitError::NoCName { symbol } => write!(f, "the unit's Mach-O global {symbol} does not start with `_`, so it has no C name {at}"),
            SplitError::SymbolOutside { symbol, section, offset } => write!(f, "the unit's symbol {symbol} is at {offset}, past the end of section {section} {at}"),
            SplitError::RefAcross { section, offset } => write!(f, "the unit's relocation at {offset} of section {section} is not inside one atom {at}"),
            SplitError::RefTarget { section, offset } => write!(f, "the unit's relocation into section {section} lands at {offset}, past its end {at}"),
            SplitError::RefInNoAtom { section, offset } => write!(f, "the unit's relocation at {offset} of section {section} is in a section no record is made of {at}"),
            SplitError::InitEntry { section, offset } => write!(f, "the unit's init entry at {offset} of section {section} is not one 8-byte pointer relocation {at}"),
            SplitError::InitPriority { section } => write!(f, "the unit's init section {section}'s suffix is not a priority {at}"),
            SplitError::UnwindNotAtAtom { section, offset } => write!(f, "the unit's unwind row names offset {offset} of section {section}, which no atom starts at {at}"),
            SplitError::UnwindTwice { section, offset } => write!(f, "the unit's atom at {offset} of section {section} has two unwind rows {at}"),
        }
    }
}

impl core::error::Error for SplitError<'_> {}

impl From<ReadError> for SplitError<'_> {
    fn from(why: ReadError) -> Self {
        SplitError::Read(why)
    }
}

/// Whether a symbol names a place: not ELF's mapping, section or file symbols.
fn is_name(symbol: &object::Symbol<'_>) -> bool {
    !symbol.name.is_empty() && symbol.name[0] != b'$'
}

/// Whether a bind is seen by other units.
fn global(bind: Bind) -> bool {
    matches!(bind, Bind::Global | Bind::Weak)
}

/// Bytes `name` without Mach-O's `_`.
fn c_name<'a>(platform: Platform, name: &'a [u8]) -> Result<&'a [u8], SplitError<'a>> {
    match platform {
        Platform::MacosArm64 => match name.split_first() {
            Some((b'_', rest)) => Ok(rest),
            _ => Err(SplitError::NoCName { symbol: Shown(name) }),
        },
        Platform::LinuxArm64 => Ok(name),
    }
}

/// The Mach-O header's flags word (offset 24 of `mach_header_64`). blaze-native has read the
/// header whole before this is asked, so the word is there.
fn macho_flags(bytes: &[u8]) -> u32 {
    let s = &bytes[24..28];
    u32::from_le_bytes([s[0], s[1], s[2], s[3]])
}

fn kind_of(kind: object::RelocKind) -> RefKind {
    match kind {
        RelocKind::Abs64 => RefKind::Abs64,
        RelocKind::Abs32 => RefKind::Abs32,
        RelocKind::Rel32 => RefKind::Rel32,
        RelocKind::Rel64 => RefKind::Rel64,
        RelocKind::Delta32 { .. } => RefKind::Delta32,
        RelocKind::Delta64 { .. } => RefKind::Delta64,
        RelocKind::Branch26 => RefKind::Branch26,
        RelocKind::CondBranch19 => RefKind::CondBranch19,
        RelocKind::TestBranch14 => RefKind::TestBranch14,
        RelocKind::Page21 => RefKind::Page21,
        RelocKind::PageOff12 { scale_log2 } => RefKind::PageOff12 { scale_log2 },
        RelocKind::GotPage21 => RefKind::GotPage21,
        RelocKind::GotPageOff12 => RefKind::GotPageOff12,
        RelocKind::GotRel32 => RefKind::GotRel32,
        RelocKind::TlvPage21 => RefKind::TlvPage21,
        RelocKind::TlvPageOff12 => RefKind::TlvPageOff12,
        RelocKind::TlsDescPage21 => RefKind::TlsDescPage21,
        RelocKind::TlsDescLo12 => RefKind::TlsDescLo12,
        RelocKind::TlsDescAdd => RefKind::TlsDescAdd,
        RelocKind::TlsDescCall => RefKind::TlsDescCall,
        RelocKind::TlsIePage21 => RefKind::TlsIePage21,
        RelocKind::TlsIeLo12 => RefKind::TlsIeLo12,
        RelocKind::TlsLeHi12 => RefKind::TlsLeHi12,
        RelocKind::TlsLeLo12 => RefKind::TlsLeLo12,
    }
}

/// The bytes a relocation of `kind` writes.
fn width(kind: RefKind) -> u64 {
    match kind {
        RefKind::Abs64 | RefKind::Rel64 | RefKind::Delta64 => 8,
        _ => 4,
    }
}

/// The work of one split: the object, its atoms so far, and the symbol-to-atom table.
struct Cut<'o, 'a> {
    object: &'o Object<'a>,
    platform: Platform,
    atoms: AVec<'a, Atom<'a>>,
    /// Per symbol, the atom it stands at (`u32::MAX`: none).
    atom_of: AVec<'a, u32>,
}

impl<'o, 'a> Cut<'o, 'a> {
    fn section_name(&self, section: u32) -> Shown<'a> {
        Shown(self.object.sections[section as usize].name)
    }

    /// The atom of `section` holding `offset`; `end` allows the section's end (an end pointer),
    /// which is the last atom's.
    fn atom_at(&self, section: u32, offset: u64, end: bool) -> Option<u32> {
        let first = self.atoms.partition_point(|a| a.section < section);
        let last = self.atoms.partition_point(|a| a.section <= section);
        let run = &self.atoms[first..last];
        let i = run.partition_point(|a| a.start <= offset);
        let atom = run.get(i.checked_sub(1)?)?;
        let inside = offset < atom.start + atom.size || (end && offset == atom.start + atom.size && i == run.len());
        inside.then_some((first + i - 1) as u32)
    }

    /// What a relocation's `to` reaches, and the addend made relative to it.
    fn target(&self, to: RelocTo, addend: i64) -> Result<(RefTo<'a>, i64), SplitError<'a>> {
        match to {
            RelocTo::Symbol(n) => {
                let symbol = &self.object.symbols[n as usize];
                match (symbol.bind, symbol.def) {
                    (Bind::Global | Bind::Weak, _) => Ok((RefTo::Name { name: c_name(self.platform, symbol.name)?, weak: false }, addend)),
                    (Bind::WeakUndefined, _) => Ok((RefTo::Name { name: c_name(self.platform, symbol.name)?, weak: true }, addend)),
                    (Bind::Local, Def::In { offset, .. }) => {
                        let atom = self.atom_of[n as usize];
                        if atom == u32::MAX {
                            return Err(SplitError::RefTarget { section: Shown(symbol.name), offset });
                        }
                        let from = (offset - self.atoms[atom as usize].start) as i64;
                        Ok((RefTo::Atom(atom), from + addend))
                    }
                    (Bind::Local, Def::Absolute(_)) => Err(SplitError::Absolute { symbol: Shown(symbol.name) }),
                    (Bind::Local, Def::Common { .. }) => Err(SplitError::Common { symbol: Shown(symbol.name) }),
                    (Bind::Local, Def::Undefined) => Err(SplitError::RefTarget { section: Shown(symbol.name), offset: 0 }),
                }
            }
            RelocTo::Section(section) => {
                let offset = u64::try_from(addend).map_err(|_| SplitError::RefTarget { section: self.section_name(section), offset: 0 })?;
                let atom = self.atom_at(section, offset, true).ok_or(SplitError::RefTarget { section: self.section_name(section), offset })?;
                Ok((RefTo::Atom(atom), (offset - self.atoms[atom as usize].start) as i64))
            }
        }
    }
}

/// Splits one unit's relocatable object (module docs). `plant` is [`blaze_own::planted::NoPlant`]
/// in shipped code.
pub fn split<'a>(bytes: &'a [u8], opts: SplitOpts<'a>, plant: impl Planted<SplitPlant>) -> Result<Split<'a>, SplitError<'a>> {
    let SplitOpts { platform, alloc } = opts;
    let object = object::read(platform, bytes, alloc)?;
    if platform == Platform::MacosArm64 && macho_flags(bytes) & MH_SUBSECTIONS_VIA_SYMBOLS == 0 {
        return Err(SplitError::NotSubsections);
    }
    let mut cut = Cut { object: &object, platform, atoms: Vec::new_in(alloc), atom_of: Vec::from_elem_in(u32::MAX, object.symbols.len(), alloc) };
    let mut aliases: AVec<'a, &'a [u8]> = Vec::new_in(alloc);

    // The places that start an atom, per section: (section, offset, symbol), symbols first in
    // file order at one place.
    let mut starts: AVec<'a, (u32, u64, u32)> = Vec::new_in(alloc);
    for (n, symbol) in object.symbols.iter().enumerate() {
        match symbol.def {
            Def::Common { .. } => return Err(SplitError::Common { symbol: Shown(symbol.name) }),
            Def::Absolute(_) if global(symbol.bind) => return Err(SplitError::Absolute { symbol: Shown(symbol.name) }),
            Def::In { section, offset } if is_name(symbol) => {
                let s = &object.sections[section as usize];
                if AtomKind::of(s.kind).is_none() {
                    continue;
                }
                if offset > s.size {
                    return Err(SplitError::SymbolOutside { symbol: Shown(symbol.name), section: Shown(s.name), offset });
                }
                starts.push((section, offset, n as u32));
            }
            _ => {}
        }
    }
    starts.sort_unstable();

    for (number, section) in object.sections.iter().enumerate() {
        let number = number as u32;
        let Some(kind) = AtomKind::of(section.kind) else {
            match section.kind {
                SectionKind::Other => return Err(SplitError::Section { section: Shown(section.name), kind: section.kind }),
                _ => continue,
            }
        };
        let first = starts.partition_point(|s| s.0 < number);
        let last = starts.partition_point(|s| s.0 <= number);
        let here = &starts[first..last];
        if platform == Platform::LinuxArm64 {
            // One atom a section: every symbol at its first byte (bytes before a symbol would be
            // a second atom the assembler may have reached with no relocation).
            if let Some(other) = here.iter().find(|s| s.1 != 0) {
                return Err(SplitError::SharedSection { section: Shown(section.name), at: other.1 });
            }
        }
        let mut offsets: AVec<'a, u64> = Vec::new_in(alloc);
        if here.first().is_none_or(|s| s.1 > 0) && section.size > 0 {
            offsets.push(0);
        }
        for s in here {
            if offsets.last() != Some(&s.1) {
                offsets.push(s.1);
            }
        }
        for (i, &start) in offsets.iter().enumerate() {
            let end = offsets.get(i + 1).copied().unwrap_or(section.size);
            let at_start: &[(u32, u64, u32)] = {
                let a = here.partition_point(|s| s.1 < start);
                let b = here.partition_point(|s| s.1 <= start);
                &here[a..b]
            };
            let index = cut.atoms.len() as u32;
            let named = at_start.iter().find(|s| global(object.symbols[s.2 as usize].bind)).or(at_start.first());
            let (name, strength, hidden) = match named {
                None => (AtomName::Anonymous { section: number, start }, Strength::Local, false),
                Some(&(_, _, n)) => {
                    let symbol = &object.symbols[n as usize];
                    let in_group = object.groups.iter().any(|g| g.sections.contains(&number));
                    match symbol.bind {
                        Bind::Global | Bind::Weak => {
                            let strength = match (in_group, symbol.bind) {
                                (true, _) => Strength::Comdat,
                                (false, Bind::Weak) => Strength::Weak,
                                (false, _) => Strength::Strong,
                            };
                            (AtomName::Global(c_name(platform, symbol.name)?), strength, symbol.hidden)
                        }
                        _ => (AtomName::Local { name: symbol.name, nth: 0 }, Strength::Local, false),
                    }
                }
            };
            let alias_start = aliases.len() as u32;
            for s in at_start {
                cut.atom_of[s.2 as usize] = index;
                let symbol = &object.symbols[s.2 as usize];
                if Some(s) != named && global(symbol.bind) {
                    aliases.push(c_name(platform, symbol.name)?);
                }
            }
            let bytes = match kind {
                AtomKind::Zero | AtomKind::ThreadZero => &[][..],
                _ => &section.bytes[start as usize..end as usize],
            };
            cut.atoms.push(Atom {
                name,
                kind,
                strength,
                hidden,
                section: number,
                start,
                size: end - start,
                align_log2: atom_align(section.align_log2, start),
                bytes,
                refs: 0..0,
                aliases: alias_start..aliases.len() as u32,
                unwind: None,
            });
        }
    }

    // A local name's count of the atoms before it that bear the same text.
    for i in 0..cut.atoms.len() {
        if let AtomName::Local { name, .. } = cut.atoms[i].name {
            let nth = cut.atoms[..i].iter().filter(|a| matches!(a.name, AtomName::Local { name: other, .. } if other == name)).count() as u32;
            cut.atoms[i].name = AtomName::Local { name, nth };
        }
    }

    let mut refs: AVec<'a, Ref<'a>> = Vec::new_in(alloc);
    let mut inits: AVec<'a, Init<'a>> = Vec::new_in(alloc);
    for (number, section) in object.sections.iter().enumerate() {
        let number = number as u32;
        let own = &object.relocs[section.relocs.start as usize..section.relocs.end as usize];
        match section.kind {
            SectionKind::InitPointers | SectionKind::TermPointers => {
                inits_of(&cut, number, own, &mut inits)?;
                continue;
            }
            SectionKind::CompactUnwind | SectionKind::EhFrame | SectionKind::Skip => continue,
            _ => {}
        }
        for (i, reloc) in own.iter().enumerate() {
            if i == 0 && plant.has(SplitPlant::DropsRef) {
                continue;
            }
            let offset = u64::from(reloc.offset);
            let kind = kind_of(reloc.kind);
            let atom = cut.atom_at(number, offset, false).ok_or(SplitError::RefInNoAtom { section: Shown(section.name), offset })?;
            let a = &cut.atoms[atom as usize];
            if offset + width(kind) > a.start + a.size {
                return Err(SplitError::RefAcross { section: Shown(section.name), offset });
            }
            let (to, addend) = cut.target(reloc.to, reloc.addend)?;
            let minus = match reloc.kind {
                RelocKind::Delta32 { minus } | RelocKind::Delta64 { minus } => Some(cut.target(minus, 0)?),
                _ => None,
            };
            let at = (offset - a.start) as u32;
            refs.push(Ref { at, kind, to, addend, minus });
            let run = &mut cut.atoms[atom as usize].refs;
            if run.start == run.end {
                *run = refs.len() as u32 - 1..refs.len() as u32;
            } else {
                run.end = refs.len() as u32;
            }
        }
    }
    // Atoms with no reference sit at the run's place, empty.
    let mut next = 0u32;
    for atom in cut.atoms.iter_mut() {
        if atom.refs.start == atom.refs.end {
            atom.refs = next..next;
        }
        next = atom.refs.end;
    }

    for row in object.unwind.iter() {
        let section = row.function.section;
        let offset = row.function.offset;
        let atom = cut.atom_at(section, offset, false).filter(|&a| cut.atoms[a as usize].start == offset);
        let atom = atom.ok_or(SplitError::UnwindNotAtAtom { section: cut.section_name(section), offset })?;
        let personality = match row.personality {
            Some(to) => Some(cut.target(to, 0)?.0),
            None => None,
        };
        let lsda = match row.lsda {
            Some(point) => {
                let at = cut.atom_at(point.section, point.offset, false).ok_or(SplitError::UnwindNotAtAtom { section: cut.section_name(point.section), offset: point.offset })?;
                Some((at, point.offset - cut.atoms[at as usize].start))
            }
            None => None,
        };
        let slot = &mut cut.atoms[atom as usize].unwind;
        if slot.is_some() {
            return Err(SplitError::UnwindTwice { section: cut.section_name(section), offset });
        }
        *slot = Some(Unwind { length: row.length, encoding: row.encoding, personality, lsda });
    }

    Ok(Split { atoms: cut.atoms, refs, aliases, inits })
}

/// An atom's alignment: its section's, lowered to what its offset in the section keeps.
fn atom_align(section_log2: u8, start: u64) -> u8 {
    if start == 0 { section_log2 } else { section_log2.min(start.trailing_zeros() as u8) }
}

/// The init rows of one init or term section: one 8-byte pointer relocation each 8 bytes.
fn inits_of<'a>(cut: &Cut<'_, 'a>, number: u32, own: &[object::Reloc], inits: &mut AVec<'a, Init<'a>>) -> Result<(), SplitError<'a>> {
    let section = &cut.object.sections[number as usize];
    let fini = section.kind == SectionKind::TermPointers;
    let priority = priority_of(section.name).ok_or(SplitError::InitPriority { section: Shown(section.name) })?;
    if section.size % 8 != 0 || own.len() as u64 != section.size / 8 {
        return Err(SplitError::InitEntry { section: Shown(section.name), offset: (own.len() as u64) * 8 });
    }
    for (i, reloc) in own.iter().enumerate() {
        if u64::from(reloc.offset) != i as u64 * 8 || reloc.kind != RelocKind::Abs64 {
            return Err(SplitError::InitEntry { section: Shown(section.name), offset: u64::from(reloc.offset) });
        }
        let (to, addend) = cut.target(reloc.to, reloc.addend)?;
        inits.push(Init { fini, priority, to, addend });
    }
    Ok(())
}

/// An init section's priority: ELF's `.init_array.N`/`.fini_array.N` suffix, else
/// [`NO_PRIORITY`]; `None` for a suffix that is not a number below it.
fn priority_of(name: &[u8]) -> Option<u32> {
    let suffix = [&b".init_array."[..], &b".fini_array."[..]].iter().find_map(|prefix| name.strip_prefix(*prefix));
    let Some(digits) = suffix else {
        return Some(NO_PRIORITY);
    };
    if digits.is_empty() || digits.len() > 5 || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let value = digits.iter().fold(0u32, |n, d| n * 10 + u32::from(d - b'0'));
    (value < NO_PRIORITY).then_some(value)
}
