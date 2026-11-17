//! The one object model both readers fill: what a relocatable object holds that a link needs.
//!
//! Indexes are the file's own, so a relocation or symbol read later points the same way the
//! file does: section `i` is Mach-O's section ordinal `i + 1` (`n_sect`, counted over every
//! `LC_SEGMENT_64` in order) or ELF's section header `i` (index 0, the null header, included);
//! symbol `i` is the file's symbol table entry `i`. Sections the image never holds (debug
//! information, the address-significance table, ELF's structural sections) are kept with
//! [`SectionKind::Skip`] so the indexes stay the file's; their relocations are not read.
//!
//! Positions in errors (`at`) are offsets in the file's bytes unless a variant says otherwise.

use core::ops::Range;

use blaze_own::{AVec, Alloc};

use crate::Platform;

/// One relocatable object, borrowing its bytes.
pub struct Object<'a> {
    pub sections: AVec<'a, Section<'a>>,
    pub symbols: AVec<'a, Symbol<'a>>,
    /// Every relocation of every section that is not [`SectionKind::Skip`], sorted by
    /// (section, offset); no two share a place.
    pub relocs: AVec<'a, Reloc>,
    /// Mach-O `__LD,__compact_unwind` rows, in section then row order; empty on ELF.
    pub unwind: AVec<'a, UnwindRow>,
    /// ELF `SHT_GROUP` COMDAT groups, in section header order; empty on Mach-O.
    pub groups: AVec<'a, Group<'a>>,
}

/// One section. `segment` is Mach-O's segment name (empty on ELF); `bytes` is the section's
/// contents in the file (empty for zero-fill kinds, whose length is `size`); `relocs` is the
/// section's run in [`Object::relocs`].
pub struct Section<'a> {
    pub segment: &'a [u8],
    pub name: &'a [u8],
    pub kind: SectionKind,
    pub align_log2: u8,
    pub bytes: &'a [u8],
    pub size: u64,
    pub relocs: Range<u32>,
}

/// What a section is to a link. `Other` is an allocated ELF section of a name and type the
/// reader does not classify (`.note.gnu.property`, `.preinit_array`, a named section kept for
/// `__start_`/`__stop_` symbols): the layout decides it and refuses it by its name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SectionKind {
    Code,
    ReadOnly,
    CStrings,
    Data,
    ZeroFill,
    ThreadData,
    ThreadZero,
    ThreadVars,
    InitPointers,
    TermPointers,
    EhFrame,
    CompactUnwind,
    ExceptTable,
    Skip,
    Other,
}

/// One symbol table entry. Names are as the file spells them (Mach-O's C names keep their
/// leading `_`). `hidden`: Mach-O `N_PEXT`, ELF `STV_HIDDEN` or `STV_INTERNAL`.
pub struct Symbol<'a> {
    pub name: &'a [u8],
    pub def: Def,
    pub bind: Bind,
    pub hidden: bool,
}

/// Where a symbol's value is. `In`'s offset is from the start of the section.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Def {
    Undefined,
    Common { size: u64, align_log2: u8 },
    Absolute(u64),
    In { section: u32, offset: u64 },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bind {
    Local,
    Global,
    Weak,
    WeakUndefined,
}

/// What a relocation refers to: a symbol, or a section's start (Mach-O's non-extern form; an
/// ELF relocation against an `STT_SECTION` symbol is read as its section, so both formats say
/// "this section plus the addend" the same way).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RelocTo {
    Symbol(u32),
    Section(u32),
}

/// The relocation's computation, named by what it writes (AArch64 ELF ABI's and Mach-O
/// `ARM64_RELOC_*`'s forms merged where they write the same field the same way).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RelocKind {
    Abs64,
    Abs32,
    Rel32,
    Rel64,
    Delta32 { minus: RelocTo },
    Delta64 { minus: RelocTo },
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

/// One relocation: at `offset` in section `section`, write `kind` of `to` plus `addend`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Reloc {
    pub section: u32,
    pub offset: u32,
    pub kind: RelocKind,
    pub to: RelocTo,
    pub addend: i64,
}

/// A place in a section.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Point {
    pub section: u32,
    pub offset: u64,
}

/// One `__LD,__compact_unwind` row (`struct compact_unwind_entry` of libunwind's
/// `compact_unwind_encoding.h`: function address, length, encoding, personality, LSDA), its
/// pointers read through their relocations.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct UnwindRow {
    pub function: Point,
    pub length: u32,
    pub encoding: u32,
    pub personality: Option<RelocTo>,
    pub lsda: Option<Point>,
}

/// One ELF COMDAT group: its signature (the named symbol's name, or the section's name when the
/// symbol is a section symbol) and its member sections.
pub struct Group<'a> {
    pub signature: &'a [u8],
    pub sections: AVec<'a, u32>,
}

/// Why an object was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReadError {
    /// The magic is not the platform's object format.
    WrongFormat { want: Platform },
    /// A read at `at` runs past the end (`u32::MAX` when the offset is itself past 4 GiB).
    Truncated { at: u32 },
    /// A form the reader does not handle, named by `what`, with its value.
    Unsupported { what: &'static str, value: u32, at: u32 },
    /// The bytes break the format's rule named by `what`.
    Malformed { what: &'static str, at: u32 },
    /// The caller's allocator refused.
    Alloc,
}

/// Reads one relocatable object for `platform`: the magic must be the platform's format
/// (Mach-O `MH_MAGIC_64` for macOS, `\x7fELF` for Linux), and the format's reader does the rest.
pub fn read<'a>(platform: Platform, bytes: &'a [u8], alloc: Alloc<'a>) -> Result<Object<'a>, ReadError> {
    match platform {
        Platform::MacosArm64 => {
            if bytes.get(0..4) != Some(&crate::macho::MAGIC_BYTES[..]) {
                return Err(ReadError::WrongFormat { want: platform });
            }
            crate::macho::read(bytes, alloc)
        }
        Platform::LinuxArm64 => {
            if bytes.get(0..4) != Some(&crate::elf::MAGIC_BYTES[..]) {
                return Err(ReadError::WrongFormat { want: platform });
            }
            crate::elf::read(bytes, alloc)
        }
    }
}

// ---- shared reading helpers (both readers) ----

/// `at` as a 32-bit error position: objects are refused at 4 GiB or more ([`fits`]), so an
/// offset inside one fits; `u32::MAX` stands for an offset read from a field that points past.
pub(crate) fn at32(at: usize) -> u32 {
    u32::try_from(at).unwrap_or(u32::MAX)
}

/// Refuses an object of 4 GiB or more: every offset in the model is 32-bit.
pub(crate) fn fits(bytes: &[u8]) -> Result<(), ReadError> {
    if u32::try_from(bytes.len()).is_err() {
        return Err(ReadError::Malformed { what: "an object of 4 GiB or more (offsets are 32-bit)", at: 0 });
    }
    Ok(())
}

/// `len` bytes at `at`, bounds-checked.
pub(crate) fn slice(bytes: &[u8], at: usize, len: usize) -> Result<&[u8], ReadError> {
    let end = at.checked_add(len).ok_or(ReadError::Truncated { at: at32(at) })?;
    bytes.get(at..end).ok_or(ReadError::Truncated { at: at32(at) })
}

pub(crate) fn u8_at(bytes: &[u8], at: usize) -> Result<u8, ReadError> {
    bytes.get(at).copied().ok_or(ReadError::Truncated { at: at32(at) })
}

pub(crate) fn u16_at(bytes: &[u8], at: usize) -> Result<u16, ReadError> {
    let s = slice(bytes, at, 2)?;
    Ok(u16::from_le_bytes([s[0], s[1]]))
}

pub(crate) fn u32_at(bytes: &[u8], at: usize) -> Result<u32, ReadError> {
    let s = slice(bytes, at, 4)?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

pub(crate) fn u64_at(bytes: &[u8], at: usize) -> Result<u64, ReadError> {
    let s = slice(bytes, at, 8)?;
    Ok(u64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
}

/// A file offset or size read from a field, as `usize`; `field_at` is where the field is.
pub(crate) fn index(value: u64, field_at: usize) -> Result<usize, ReadError> {
    usize::try_from(value).map_err(|_| ReadError::Truncated { at: at32(field_at) })
}

/// `a + b * c` for offsets computed from fields, refused as truncated at `field_at` on overflow.
pub(crate) fn offset(a: usize, b: usize, c: usize, field_at: usize) -> Result<usize, ReadError> {
    b.checked_mul(c).and_then(|bc| a.checked_add(bc)).ok_or(ReadError::Truncated { at: at32(field_at) })
}

/// The NUL-ended name at `strx` in a string table; `at` is the field that named it.
pub(crate) fn name_at(table: &[u8], strx: u32, at: usize) -> Result<&[u8], ReadError> {
    let rest = table.get(strx as usize..).ok_or(ReadError::Malformed { what: "a name's offset is outside its string table", at: at32(at) })?;
    let end = rest.iter().position(|&b| b == 0).ok_or(ReadError::Malformed { what: "a name runs past the end of its string table", at: at32(at) })?;
    Ok(&rest[..end])
}

pub(crate) fn push<T>(into: &mut AVec<'_, T>, value: T) -> Result<(), ReadError> {
    into.try_push(value).map_err(|_| ReadError::Alloc)
}

/// The bytes a relocation of `kind` writes at its offset.
pub(crate) fn width(kind: RelocKind) -> usize {
    match kind {
        RelocKind::Abs64 | RelocKind::Rel64 | RelocKind::Delta64 { .. } => 8,
        _ => 4,
    }
}

/// Sorts the relocations by (section, offset), refuses two at one place, and gives each section
/// its run. `Malformed`'s `at` here is the place's offset in its section.
pub(crate) fn finish_relocs(relocs: &mut AVec<'_, Reloc>, sections: &mut [Section<'_>]) -> Result<(), ReadError> {
    if u32::try_from(relocs.len()).is_err() {
        return Err(ReadError::Malformed { what: "more than 2^32 relocations", at: 0 });
    }
    relocs.sort_unstable_by_key(|r| (r.section, r.offset));
    for pair in relocs.windows(2) {
        if pair[0].section == pair[1].section && pair[0].offset == pair[1].offset {
            return Err(ReadError::Malformed { what: "two relocations at one place of a section", at: pair[1].offset });
        }
    }
    let mut next = 0usize;
    for (number, section) in sections.iter_mut().enumerate() {
        let start = next;
        while next < relocs.len() && relocs[next].section as usize == number {
            next += 1;
        }
        // Both fit: the count was checked above.
        section.relocs = at32(start)..at32(next);
    }
    if next != relocs.len() {
        return Err(ReadError::Malformed { what: "a relocation names a section past the last", at: relocs[next].offset });
    }
    Ok(())
}

/// Checks a relocation's place lies inside its section: `offset + width(kind) <= size`.
pub(crate) fn place_fits(offset: u64, kind: RelocKind, size: u64, at: usize) -> Result<u32, ReadError> {
    let end = offset.checked_add(width(kind) as u64);
    match (u32::try_from(offset), end) {
        (Ok(offset), Some(end)) if end <= size => Ok(offset),
        _ => Err(ReadError::Malformed { what: "a relocation's place is outside its section", at: at32(at) }),
    }
}
