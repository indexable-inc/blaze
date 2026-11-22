//! Mach-O relocatable objects for arm64 (`MH_OBJECT`, `CPU_TYPE_ARM64`), as clang writes them.
//!
//! Structures from Apple's `<mach-o/loader.h>`, `<mach-o/nlist.h>`, `<mach-o/reloc.h>` and
//! `<mach-o/arm64/reloc.h>` (the cctools / ld64 headers); the meaning of each `ARM64_RELOC_*`
//! as ld64 and lld's MachO port read it:
//!
//! - `mach_header_64` (32 bytes), then `ncmds` load commands in `sizeofcmds` bytes;
//! - `LC_SEGMENT_64` (72 bytes) and its `section_64`s (80 bytes each); a section's ordinal is its
//!   position over all segments, from 1 (`n_sect`, a non-extern relocation's `r_symbolnum`);
//! - `LC_SYMTAB`: `nlist_64` (16 bytes: `n_strx`, `n_type`, `n_sect`, `n_desc`, `n_value`) and
//!   the string table; `n_value` of an `N_SECT` symbol is an address in the object's own
//!   address space, so its offset is `n_value - section.addr`;
//! - `relocation_info` (8 bytes: `r_address`, then `r_symbolnum:24 r_pcrel:1 r_length:2
//!   r_extern:1 r_type:4`); `ARM64_RELOC_ADDEND` gives the next relocation its addend (in
//!   `r_symbolnum`, 24 bits signed); `ARM64_RELOC_SUBTRACTOR` names the subtracted symbol and the
//!   `ARM64_RELOC_UNSIGNED` right after it the added one; an `UNSIGNED`'s addend is the bytes at
//!   its place, and a non-extern one's bytes hold the target's address, so its addend is that
//!   minus the section's address. Instruction relocations carry no addend in the instruction.
//!
//! Load commands besides `LC_SEGMENT_64` and `LC_SYMTAB`: `LC_DYSYMTAB` (an index over the
//! symbol table, which is read whole), `LC_BUILD_VERSION` and `LC_VERSION_MIN_MACOSX` (the
//! deployment target, which the program's own record states), `LC_LINKER_OPTIMIZATION_HINT`
//! (hints a linker MAY use to relax ADRP sequences; skipping them leaves correct code) and
//! `LC_DATA_IN_CODE` (marks data inside code for disassemblers) are read and ignored;
//! `LC_LINKER_OPTION` only when empty (its strings are `-l`/`-framework` options the link would
//! have to obey); any other is refused by number.

use blaze_own::{AVec, Alloc, Vec};

use crate::Platform;
use crate::object::{
    Bind, Def, Object, Point, ReadError, Reloc, RelocKind, RelocTo, Section, SectionKind, Symbol, UnwindRow, at32, finish_relocs, fits, index,
    name_at, offset, place_fits, push, slice, u8_at, u16_at, u32_at, u64_at,
};

/// `MH_MAGIC_64` as it stands in a little-endian file.
pub(crate) const MAGIC_BYTES: [u8; 4] = [0xcf, 0xfa, 0xed, 0xfe];

const MH_MAGIC_64: u32 = 0xfeed_facf;
const CPU_TYPE_ARM64: u32 = 0x0100_000c;
const MH_OBJECT: u32 = 0x1;
const HEADER_LEN: usize = 32;

const LC_SYMTAB: u32 = 0x2;
const LC_DYSYMTAB: u32 = 0xb;
const LC_SEGMENT_64: u32 = 0x19;
const LC_VERSION_MIN_MACOSX: u32 = 0x24;
const LC_DATA_IN_CODE: u32 = 0x29;
const LC_LINKER_OPTION: u32 = 0x2d;
const LC_LINKER_OPTIMIZATION_HINT: u32 = 0x2e;
const LC_BUILD_VERSION: u32 = 0x32;

const SEGMENT_LEN: usize = 72;
const SECTION_LEN: usize = 80;
const NLIST_LEN: usize = 16;
const RELOC_LEN: usize = 8;
const COMPACT_UNWIND_ROW: usize = 32;

const SECTION_TYPE: u32 = 0x0000_00ff;
const S_REGULAR: u32 = 0x0;
const S_ZEROFILL: u32 = 0x1;
const S_CSTRING_LITERALS: u32 = 0x2;
const S_4BYTE_LITERALS: u32 = 0x3;
const S_8BYTE_LITERALS: u32 = 0x4;
const S_MOD_INIT_FUNC_POINTERS: u32 = 0x9;
const S_MOD_TERM_FUNC_POINTERS: u32 = 0xa;
const S_COALESCED: u32 = 0xb;
const S_GB_ZEROFILL: u32 = 0xc;
const S_16BYTE_LITERALS: u32 = 0xe;
const S_THREAD_LOCAL_REGULAR: u32 = 0x11;
const S_THREAD_LOCAL_ZEROFILL: u32 = 0x12;
const S_THREAD_LOCAL_VARIABLES: u32 = 0x13;
const S_ATTR_PURE_INSTRUCTIONS: u32 = 0x8000_0000;
const S_ATTR_DEBUG: u32 = 0x0200_0000;
const S_ATTR_SOME_INSTRUCTIONS: u32 = 0x0000_0400;

const N_STAB: u8 = 0xe0;
const N_PEXT: u8 = 0x10;
const N_TYPE: u8 = 0x0e;
const N_EXT: u8 = 0x01;
const N_UNDF: u8 = 0x0;
const N_ABS: u8 = 0x2;
const N_SECT: u8 = 0xe;
const N_WEAK_REF: u16 = 0x0040;
const N_WEAK_DEF: u16 = 0x0080;

const ARM64_RELOC_UNSIGNED: u32 = 0;
const ARM64_RELOC_SUBTRACTOR: u32 = 1;
const ARM64_RELOC_BRANCH26: u32 = 2;
const ARM64_RELOC_PAGE21: u32 = 3;
const ARM64_RELOC_PAGEOFF12: u32 = 4;
const ARM64_RELOC_GOT_LOAD_PAGE21: u32 = 5;
const ARM64_RELOC_GOT_LOAD_PAGEOFF12: u32 = 6;
const ARM64_RELOC_POINTER_TO_GOT: u32 = 7;
const ARM64_RELOC_TLVP_LOAD_PAGE21: u32 = 8;
const ARM64_RELOC_TLVP_LOAD_PAGEOFF12: u32 = 9;
const ARM64_RELOC_ADDEND: u32 = 10;

/// One `relocation_info`, unpacked, with where it stands in the file.
#[derive(Clone, Copy)]
struct Raw {
    address: u32,
    symbolnum: u32,
    pcrel: bool,
    length: u32,
    external: bool,
    kind: u32,
    at: usize,
}

/// A section's relocation table, read after the symbols.
#[derive(Clone, Copy)]
struct Table {
    section: u32,
    reloff: usize,
    nreloc: usize,
}

/// Reads one Mach-O arm64 relocatable object in place.
pub fn read<'a>(bytes: &'a [u8], alloc: Alloc<'a>) -> Result<Object<'a>, ReadError> {
    fits(bytes)?;
    if u32_at(bytes, 0)? != MH_MAGIC_64 {
        return Err(ReadError::WrongFormat { want: Platform::MacosArm64 });
    }
    let cpu = u32_at(bytes, 4)?;
    if cpu != CPU_TYPE_ARM64 {
        return Err(ReadError::Unsupported { what: "mach-o cputype (CPU_TYPE_ARM64 only)", value: cpu, at: 4 });
    }
    let filetype = u32_at(bytes, 12)?;
    if filetype != MH_OBJECT {
        return Err(ReadError::Unsupported { what: "mach-o filetype (MH_OBJECT only)", value: filetype, at: 12 });
    }
    let ncmds = u32_at(bytes, 16)?;
    let sizeofcmds = u32_at(bytes, 20)? as usize;
    let cmds_end = offset(HEADER_LEN, sizeofcmds, 1, 20)?;
    slice(bytes, HEADER_LEN, sizeofcmds)?;

    let mut sections: AVec<'a, Section<'a>> = Vec::new_in(alloc);
    let mut addresses: AVec<'a, u64> = Vec::new_in(alloc);
    let mut tables: AVec<'a, Table> = Vec::new_in(alloc);
    let mut symtab: Option<(usize, usize, usize, usize, usize)> = None;

    let mut at = HEADER_LEN;
    for _ in 0..ncmds {
        let cmd = u32_at(bytes, at)?;
        let size = u32_at(bytes, at + 4)? as usize;
        if size < 8 || size % 8 != 0 || at.checked_add(size).is_none_or(|end| end > cmds_end) {
            return Err(ReadError::Malformed { what: "a mach-o load command's cmdsize", at: at32(at) });
        }
        match cmd {
            LC_SEGMENT_64 => {
                if size < SEGMENT_LEN {
                    return Err(ReadError::Malformed { what: "LC_SEGMENT_64 shorter than segment_command_64", at: at32(at) });
                }
                let nsects = u32_at(bytes, at + 64)? as usize;
                if offset(SEGMENT_LEN, nsects, SECTION_LEN, at + 64)? > size {
                    return Err(ReadError::Malformed { what: "LC_SEGMENT_64's sections run past its cmdsize", at: at32(at) });
                }
                for n in 0..nsects {
                    read_section(bytes, at + SEGMENT_LEN + n * SECTION_LEN, &mut sections, &mut addresses, &mut tables)?;
                }
            }
            LC_SYMTAB => {
                if symtab.is_some() {
                    return Err(ReadError::Malformed { what: "a second LC_SYMTAB", at: at32(at) });
                }
                let symoff = u32_at(bytes, at + 8)? as usize;
                let nsyms = u32_at(bytes, at + 12)? as usize;
                let stroff = u32_at(bytes, at + 16)? as usize;
                let strsize = u32_at(bytes, at + 20)? as usize;
                symtab = Some((symoff, nsyms, stroff, strsize, at));
            }
            LC_DYSYMTAB | LC_BUILD_VERSION | LC_VERSION_MIN_MACOSX | LC_LINKER_OPTIMIZATION_HINT | LC_DATA_IN_CODE => {}
            LC_LINKER_OPTION => {
                let count = u32_at(bytes, at + 8)?;
                if count != 0 {
                    return Err(ReadError::Unsupported { what: "LC_LINKER_OPTION", value: count, at: at32(at) });
                }
            }
            other => return Err(ReadError::Unsupported { what: "mach-o load command", value: other, at: at32(at) }),
        }
        at += size;
    }

    let mut symbols: AVec<'a, Symbol<'a>> = Vec::new_in(alloc);
    if let Some((symoff, nsyms, stroff, strsize, cmd_at)) = symtab {
        let strtab = slice(bytes, stroff, strsize)?;
        slice(bytes, symoff, offset(0, nsyms, NLIST_LEN, cmd_at + 12)?)?;
        for n in 0..nsyms {
            let symbol = read_symbol(bytes, symoff + n * NLIST_LEN, strtab, &addresses)?;
            push(&mut symbols, symbol)?;
        }
    }

    let mut relocs: AVec<'a, Reloc> = Vec::new_in(alloc);
    for table in tables.iter() {
        read_relocs(bytes, *table, &sections, &addresses, symbols.len(), &mut relocs)?;
    }
    finish_relocs(&mut relocs, &mut sections)?;

    let mut unwind: AVec<'a, UnwindRow> = Vec::new_in(alloc);
    for section in sections.iter() {
        if section.kind == SectionKind::CompactUnwind {
            read_compact_unwind(section, &relocs, &symbols, &mut unwind)?;
        }
    }

    Ok(Object { sections, symbols, relocs, unwind, groups: Vec::new_in(alloc) })
}

/// One `section_64` at `at`.
fn read_section<'a>(
    bytes: &'a [u8],
    at: usize,
    sections: &mut AVec<'a, Section<'a>>,
    addresses: &mut AVec<'a, u64>,
    tables: &mut AVec<'a, Table>,
) -> Result<(), ReadError> {
    let name = trim_nul(slice(bytes, at, 16)?);
    let segment = trim_nul(slice(bytes, at + 16, 16)?);
    let addr = u64_at(bytes, at + 32)?;
    let size = u64_at(bytes, at + 40)?;
    let file_offset = u32_at(bytes, at + 48)? as usize;
    let align = u32_at(bytes, at + 52)?;
    let reloff = u32_at(bytes, at + 56)? as usize;
    let nreloc = u32_at(bytes, at + 60)? as usize;
    let flags = u32_at(bytes, at + 64)?;
    let kind = kind_of(segment, name, flags, at)?;
    let align_log2 = u8::try_from(align).ok().filter(|&a| a < 64).ok_or(ReadError::Malformed { what: "a mach-o section's align (log2) is 64 or more", at: at32(at + 52) })?;
    let contents = match kind {
        SectionKind::ZeroFill | SectionKind::ThreadZero => &[][..],
        _ => slice(bytes, file_offset, index(size, at + 40)?)?,
    };
    let number = u32::try_from(sections.len()).map_err(|_| ReadError::Malformed { what: "too many mach-o sections", at: at32(at) })?;
    if number >= 255 {
        // `n_sect` is one byte and counts from 1: a 256th section could not be named.
        return Err(ReadError::Malformed { what: "more than 255 mach-o sections (n_sect is one byte)", at: at32(at) });
    }
    push(sections, Section { segment, name, kind, align_log2, bytes: contents, size, relocs: 0..0 })?;
    push(addresses, addr)?;
    if kind != SectionKind::Skip && nreloc > 0 {
        push(tables, Table { section: number, reloff, nreloc })?;
    }
    Ok(())
}

/// A section's kind: by name for the sections that mean something by name, then by
/// `S_ATTR_DEBUG`, then by type.
fn kind_of(segment: &[u8], name: &[u8], flags: u32, at: usize) -> Result<SectionKind, ReadError> {
    match (segment, name) {
        (b"__TEXT", b"__eh_frame") => return Ok(SectionKind::EhFrame),
        (b"__LD", b"__compact_unwind") => return Ok(SectionKind::CompactUnwind),
        (b"__TEXT", b"__gcc_except_tab") => return Ok(SectionKind::ExceptTable),
        (b"__LLVM", b"__addrsig") => return Ok(SectionKind::Skip),
        _ => {}
    }
    if segment == b"__DWARF" || flags & S_ATTR_DEBUG != 0 {
        return Ok(SectionKind::Skip);
    }
    if segment == b"__LLVM" {
        return Ok(SectionKind::Other);
    }
    Ok(match flags & SECTION_TYPE {
        S_REGULAR | S_COALESCED | S_4BYTE_LITERALS | S_8BYTE_LITERALS | S_16BYTE_LITERALS => {
            if flags & (S_ATTR_PURE_INSTRUCTIONS | S_ATTR_SOME_INSTRUCTIONS) != 0 {
                SectionKind::Code
            } else if segment == b"__TEXT" {
                SectionKind::ReadOnly
            } else {
                SectionKind::Data
            }
        }
        S_ZEROFILL | S_GB_ZEROFILL => SectionKind::ZeroFill,
        S_CSTRING_LITERALS => SectionKind::CStrings,
        S_THREAD_LOCAL_REGULAR => SectionKind::ThreadData,
        S_THREAD_LOCAL_ZEROFILL => SectionKind::ThreadZero,
        S_THREAD_LOCAL_VARIABLES => SectionKind::ThreadVars,
        S_MOD_INIT_FUNC_POINTERS => SectionKind::InitPointers,
        S_MOD_TERM_FUNC_POINTERS => SectionKind::TermPointers,
        other => return Err(ReadError::Unsupported { what: "mach-o section type", value: other, at: at32(at + 64) }),
    })
}

/// One `nlist_64` at `at`.
fn read_symbol<'a>(bytes: &'a [u8], at: usize, strtab: &'a [u8], addresses: &[u64]) -> Result<Symbol<'a>, ReadError> {
    let strx = u32_at(bytes, at)?;
    let n_type = u8_at(bytes, at + 4)?;
    let n_sect = u8_at(bytes, at + 5)?;
    let n_desc = u16_at(bytes, at + 6)?;
    let n_value = u64_at(bytes, at + 8)?;
    if n_type & N_STAB != 0 {
        return Err(ReadError::Unsupported { what: "mach-o stab (debugging) symbol", value: u32::from(n_type), at: at32(at + 4) });
    }
    let name = name_at(strtab, strx, at)?;
    let external = n_type & N_EXT != 0;
    let def = match n_type & N_TYPE {
        N_UNDF if external && n_value != 0 => {
            // A tentative definition: `n_value` is the size, `GET_COMM_ALIGN(n_desc)` the
            // alignment's log2 (`<mach-o/nlist.h>`).
            Def::Common { size: n_value, align_log2: ((n_desc >> 8) & 0x0f) as u8 }
        }
        N_UNDF => Def::Undefined,
        N_ABS => Def::Absolute(n_value),
        N_SECT => {
            let section = usize::from(n_sect).checked_sub(1).filter(|&s| s < addresses.len()).ok_or(ReadError::Malformed {
                what: "an N_SECT symbol's n_sect names no section",
                at: at32(at + 5),
            })?;
            let base = addresses[section];
            let offset = n_value.checked_sub(base).ok_or(ReadError::Malformed { what: "an N_SECT symbol's n_value is below its section's address", at: at32(at + 8) })?;
            Def::In { section: section as u32, offset }
        }
        other => return Err(ReadError::Unsupported { what: "mach-o symbol type (N_UNDF, N_ABS, N_SECT only)", value: u32::from(other), at: at32(at + 4) }),
    };
    if def == Def::Undefined && !external {
        return Err(ReadError::Malformed { what: "an undefined mach-o symbol that is not N_EXT", at: at32(at + 4) });
    }
    let bind = match (external, def) {
        (false, _) => Bind::Local,
        (true, Def::Undefined) if n_desc & N_WEAK_REF != 0 => Bind::WeakUndefined,
        (true, Def::Undefined) => Bind::Global,
        (true, _) if n_desc & N_WEAK_DEF != 0 => Bind::Weak,
        (true, _) => Bind::Global,
    };
    Ok(Symbol { name, def, bind, hidden: n_type & N_PEXT != 0 })
}

fn raw_at(bytes: &[u8], at: usize) -> Result<Raw, ReadError> {
    let address = u32_at(bytes, at)?;
    let word = u32_at(bytes, at + 4)?;
    if address & 0x8000_0000 != 0 {
        return Err(ReadError::Unsupported { what: "mach-o scattered relocation (R_SCATTERED)", value: address, at: at32(at) });
    }
    Ok(Raw {
        address,
        symbolnum: word & 0x00ff_ffff,
        pcrel: (word >> 24) & 1 != 0,
        length: (word >> 25) & 3,
        external: (word >> 27) & 1 != 0,
        kind: word >> 28,
        at,
    })
}

/// One section's relocation table, ADDEND and SUBTRACTOR pairs folded into one [`Reloc`] each.
fn read_relocs(bytes: &[u8], table: Table, sections: &[Section<'_>], addresses: &[u64], nsyms: usize, relocs: &mut AVec<'_, Reloc>) -> Result<(), ReadError> {
    slice(bytes, table.reloff, offset(0, table.nreloc, RELOC_LEN, table.reloff)?)?;
    let section = &sections[table.section as usize];
    let mut n = 0usize;
    while n < table.nreloc {
        let raw = raw_at(bytes, table.reloff + n * RELOC_LEN)?;
        let paired = match raw.kind {
            ARM64_RELOC_ADDEND | ARM64_RELOC_SUBTRACTOR => {
                if n + 1 >= table.nreloc {
                    return Err(ReadError::Malformed { what: "ARM64_RELOC_ADDEND or SUBTRACTOR is the last relocation of its section", at: at32(raw.at) });
                }
                let next = raw_at(bytes, table.reloff + (n + 1) * RELOC_LEN)?;
                if next.address != raw.address {
                    return Err(ReadError::Malformed { what: "ARM64_RELOC_ADDEND or SUBTRACTOR's next relocation is at another address", at: at32(next.at) });
                }
                Some(next)
            }
            _ => None,
        };
        let reloc = match (raw.kind, paired) {
            (ARM64_RELOC_ADDEND, Some(next)) => {
                if !matches!(next.kind, ARM64_RELOC_BRANCH26 | ARM64_RELOC_PAGE21 | ARM64_RELOC_PAGEOFF12) {
                    return Err(ReadError::Unsupported { what: "ARM64_RELOC_ADDEND before a relocation other than BRANCH26, PAGE21 or PAGEOFF12", value: next.kind, at: at32(next.at) });
                }
                // `r_symbolnum` holds the addend, 24 bits, sign-extended.
                let addend = i64::from(((raw.symbolnum << 8) as i32) >> 8);
                n += 2;
                one(next, addend, table.section, section, addresses, nsyms)?
            }
            (ARM64_RELOC_SUBTRACTOR, Some(next)) => {
                n += 2;
                delta(raw, next, table.section, section, addresses, nsyms)?
            }
            _ => {
                n += 1;
                one(raw, 0, table.section, section, addresses, nsyms)?
            }
        };
        push(relocs, reloc)?;
    }
    Ok(())
}

/// What a relocation refers to, and the address to take off a non-extern `UNSIGNED`'s bytes.
fn target(raw: Raw, addresses: &[u64], nsyms: usize) -> Result<(RelocTo, u64), ReadError> {
    if raw.external {
        if raw.symbolnum as usize >= nsyms {
            return Err(ReadError::Malformed { what: "a mach-o relocation's r_symbolnum names no symbol", at: at32(raw.at) });
        }
        return Ok((RelocTo::Symbol(raw.symbolnum), 0));
    }
    if raw.symbolnum == 0 {
        return Err(ReadError::Unsupported { what: "mach-o R_ABS relocation (non-extern, section 0)", value: raw.kind, at: at32(raw.at) });
    }
    let section = (raw.symbolnum - 1) as usize;
    let base = *addresses.get(section).ok_or(ReadError::Malformed { what: "a non-extern mach-o relocation's r_symbolnum names no section", at: at32(raw.at) })?;
    Ok((RelocTo::Section(raw.symbolnum - 1), base))
}

/// The signed value of `width` bytes at a relocation's place (an `UNSIGNED`'s addend).
fn embedded(section: &Section<'_>, place: u32, width: usize, at: usize) -> Result<i64, ReadError> {
    let place = place as usize;
    let truncated = ReadError::Malformed { what: "a mach-o relocation's place is outside its section's bytes", at: at32(at) };
    match width {
        4 => {
            let s = section.bytes.get(place..place + 4).ok_or(truncated)?;
            Ok(i64::from(i32::from_le_bytes([s[0], s[1], s[2], s[3]])))
        }
        _ => {
            let s = section.bytes.get(place..place + 8).ok_or(truncated)?;
            Ok(i64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]))
        }
    }
}

/// `pcrel` and `length` as the type requires them; otherwise the relocation is malformed.
fn shape(raw: Raw, pcrel: bool, length: u32) -> Result<(), ReadError> {
    if raw.pcrel != pcrel || raw.length != length {
        return Err(ReadError::Malformed { what: "a mach-o relocation's r_pcrel or r_length does not fit its r_type", at: at32(raw.at) });
    }
    Ok(())
}

/// One relocation that is not part of a SUBTRACTOR pair; `addend` is an ADDEND's, or 0.
fn one(raw: Raw, addend: i64, number: u32, section: &Section<'_>, addresses: &[u64], nsyms: usize) -> Result<Reloc, ReadError> {
    let kind = match raw.kind {
        ARM64_RELOC_UNSIGNED => {
            if raw.pcrel {
                return Err(ReadError::Unsupported { what: "mach-o UNSIGNED with r_pcrel", value: raw.kind, at: at32(raw.at) });
            }
            match raw.length {
                2 => RelocKind::Abs32,
                3 => RelocKind::Abs64,
                other => return Err(ReadError::Unsupported { what: "mach-o UNSIGNED r_length (2 or 3 only)", value: other, at: at32(raw.at) }),
            }
        }
        ARM64_RELOC_BRANCH26 => {
            shape(raw, true, 2)?;
            RelocKind::Branch26
        }
        ARM64_RELOC_PAGE21 => {
            shape(raw, true, 2)?;
            RelocKind::Page21
        }
        ARM64_RELOC_PAGEOFF12 => {
            shape(raw, false, 2)?;
            RelocKind::PageOff12 { scale_log2: 0 }
        }
        ARM64_RELOC_GOT_LOAD_PAGE21 => {
            shape(raw, true, 2)?;
            RelocKind::GotPage21
        }
        ARM64_RELOC_GOT_LOAD_PAGEOFF12 => {
            shape(raw, false, 2)?;
            RelocKind::GotPageOff12
        }
        ARM64_RELOC_POINTER_TO_GOT => {
            if !raw.pcrel {
                return Err(ReadError::Unsupported { what: "mach-o POINTER_TO_GOT without r_pcrel", value: raw.kind, at: at32(raw.at) });
            }
            shape(raw, true, 2)?;
            RelocKind::GotRel32
        }
        ARM64_RELOC_TLVP_LOAD_PAGE21 => {
            shape(raw, true, 2)?;
            RelocKind::TlvPage21
        }
        ARM64_RELOC_TLVP_LOAD_PAGEOFF12 => {
            shape(raw, false, 2)?;
            RelocKind::TlvPageOff12
        }
        other => return Err(ReadError::Unsupported { what: "mach-o reloc", value: other, at: at32(raw.at) }),
    };
    let place = place_fits(u64::from(raw.address), kind, section.size, raw.at)?;
    if !raw.external && raw.kind != ARM64_RELOC_UNSIGNED {
        // clang writes section-relative (non-extern) arm64 relocations only for pointer-sized
        // data; an instruction's target is always a symbol (its `ltmp` symbols exist for this).
        return Err(ReadError::Unsupported { what: "mach-o non-extern relocation other than UNSIGNED", value: raw.kind, at: at32(raw.at) });
    }
    let (to, base) = target(raw, addresses, nsyms)?;
    let (kind, addend) = match kind {
        RelocKind::Abs32 | RelocKind::Abs64 => {
            let value = embedded(section, place, crate::object::width(kind), raw.at)?;
            (kind, value.checked_sub_unsigned(base).ok_or(ReadError::Malformed { what: "a non-extern UNSIGNED's bytes are below its section's address", at: at32(raw.at) })?)
        }
        RelocKind::PageOff12 { .. } => (RelocKind::PageOff12 { scale_log2: pageoff_scale(section, place, raw.at)? }, addend),
        _ => (kind, addend),
    };
    Ok(Reloc { section: number, offset: place, kind, to, addend })
}

/// A SUBTRACTOR (`minus`) and the UNSIGNED after it (`plus`): `plus - minus + addend`, the
/// addend being the bytes at the place.
fn delta(minus: Raw, plus: Raw, number: u32, section: &Section<'_>, addresses: &[u64], nsyms: usize) -> Result<Reloc, ReadError> {
    if plus.kind != ARM64_RELOC_UNSIGNED {
        return Err(ReadError::Malformed { what: "ARM64_RELOC_SUBTRACTOR not followed by UNSIGNED", at: at32(plus.at) });
    }
    if minus.pcrel || plus.pcrel || minus.length != plus.length {
        return Err(ReadError::Malformed { what: "a SUBTRACTOR pair's r_pcrel or r_length differ", at: at32(minus.at) });
    }
    if !minus.external {
        return Err(ReadError::Unsupported { what: "mach-o non-extern SUBTRACTOR", value: minus.kind, at: at32(minus.at) });
    }
    let (minus_to, _) = target(minus, addresses, nsyms)?;
    let kind = match minus.length {
        2 => RelocKind::Delta32 { minus: minus_to },
        3 => RelocKind::Delta64 { minus: minus_to },
        other => return Err(ReadError::Unsupported { what: "mach-o SUBTRACTOR r_length (2 or 3 only)", value: other, at: at32(minus.at) }),
    };
    let place = place_fits(u64::from(plus.address), kind, section.size, plus.at)?;
    let (to, base) = target(plus, addresses, nsyms)?;
    let value = embedded(section, place, crate::object::width(kind), plus.at)?;
    let addend = value.checked_sub_unsigned(base).ok_or(ReadError::Malformed { what: "a SUBTRACTOR pair's bytes are below its section's address", at: at32(plus.at) })?;
    Ok(Reloc { section: number, offset: place, kind, to, addend })
}

/// PAGEOFF12's scale, read from the instruction: an ADD (immediate) takes the low 12 bits as
/// they are (0); a load or store (unsigned immediate) scales them by its access size, `size` in
/// bits 30-31, and 4 for the 128-bit SIMD form (`size` 0 with `V` and `opc<1>` set) (Arm ARM,
/// "Load/store register (unsigned immediate)"; lld's `encodePageOff12`).
fn pageoff_scale(section: &Section<'_>, place: u32, at: usize) -> Result<u8, ReadError> {
    let p = place as usize;
    let s = section.bytes.get(p..p + 4).ok_or(ReadError::Malformed { what: "a PAGEOFF12's place is outside its section's bytes", at: at32(at) })?;
    let insn = u32::from_le_bytes([s[0], s[1], s[2], s[3]]);
    if insn & 0x1f00_0000 == 0x1100_0000 {
        return Ok(0);
    }
    if insn & 0x3b00_0000 == 0x3900_0000 {
        let size = (insn >> 30) as u8;
        if size == 0 && insn & 0x0480_0000 == 0x0480_0000 {
            return Ok(4);
        }
        return Ok(size);
    }
    Err(ReadError::Unsupported { what: "PAGEOFF12 on an instruction that is neither ADD (immediate) nor a load or store (unsigned immediate)", value: insn, at: at32(at) })
}

/// The place an Abs64 relocation points at: a section plus the addend, or a defined symbol
/// plus the addend.
fn point_of(reloc: &Reloc, symbols: &[Symbol<'_>], what: &'static str) -> Result<Point, ReadError> {
    let malformed = ReadError::Malformed { what, at: reloc.offset };
    let (section, base) = match reloc.to {
        RelocTo::Section(s) => (s, 0u64),
        RelocTo::Symbol(k) => match symbols.get(k as usize).map(|s| s.def) {
            Some(Def::In { section, offset }) => (section, offset),
            _ => return Err(malformed),
        },
    };
    let offset = base.checked_add_signed(reloc.addend).ok_or(malformed)?;
    Ok(Point { section, offset })
}

/// The rows of one `__LD,__compact_unwind` section; `Malformed`'s `at` here is the row's
/// offset in the section.
fn read_compact_unwind(section: &Section<'_>, relocs: &[Reloc], symbols: &[Symbol<'_>], unwind: &mut AVec<'_, UnwindRow>) -> Result<(), ReadError> {
    if section.bytes.len() % COMPACT_UNWIND_ROW != 0 {
        return Err(ReadError::Malformed { what: "__compact_unwind's size is not a multiple of 32", at: 0 });
    }
    let own = &relocs[section.relocs.start as usize..section.relocs.end as usize];
    let at = |offset: usize| own.binary_search_by_key(&offset, |r| r.offset as usize).ok().map(|i| own[i]);
    let word = |offset: usize| -> u64 {
        let s = &section.bytes[offset..offset + 8];
        u64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]])
    };
    let half = |offset: usize| -> u32 {
        let s = &section.bytes[offset..offset + 4];
        u32::from_le_bytes([s[0], s[1], s[2], s[3]])
    };
    for row in 0..section.bytes.len() / COMPACT_UNWIND_ROW {
        let base = row * COMPACT_UNWIND_ROW;
        let row_at = at32(base);
        let function = match at(base) {
            Some(r) if r.kind == RelocKind::Abs64 => point_of(&r, symbols, "a compact unwind row's function is not defined in this object")?,
            _ => return Err(ReadError::Malformed { what: "a compact unwind row without an UNSIGNED 64-bit relocation for its function", at: row_at }),
        };
        let personality = match at(base + 16) {
            Some(r) if r.kind == RelocKind::Abs64 && r.addend == 0 => Some(r.to),
            Some(_) => return Err(ReadError::Malformed { what: "a compact unwind row's personality relocation is not an UNSIGNED 64-bit with no addend", at: row_at }),
            None if word(base + 16) == 0 => None,
            None => return Err(ReadError::Malformed { what: "a compact unwind row's personality is nonzero with no relocation", at: row_at }),
        };
        let lsda = match at(base + 24) {
            Some(r) if r.kind == RelocKind::Abs64 => Some(point_of(&r, symbols, "a compact unwind row's LSDA is not defined in this object")?),
            Some(_) => return Err(ReadError::Malformed { what: "a compact unwind row's LSDA relocation is not an UNSIGNED 64-bit", at: row_at }),
            None if word(base + 24) == 0 => None,
            None => return Err(ReadError::Malformed { what: "a compact unwind row's LSDA is nonzero with no relocation", at: row_at }),
        };
        if at(base + 8).is_some() || at(base + 12).is_some() {
            return Err(ReadError::Malformed { what: "a relocation on a compact unwind row's length or encoding", at: row_at });
        }
        push(unwind, UnwindRow { function, length: half(base + 8), encoding: half(base + 12), personality, lsda })?;
    }
    Ok(())
}

fn trim_nul(field: &[u8]) -> &[u8] {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    &field[..end]
}
