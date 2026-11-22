//! ELF relocatable objects for AArch64 (ELFCLASS64, ELFDATA2LSB, `ET_REL`, `EM_AARCH64`).
//!
//! Structures from the System V ABI's "Object Files" chapter (the gABI: `Elf64_Ehdr`,
//! `Elf64_Shdr`, `Elf64_Sym`, `Elf64_Rela`, section groups, extended section numbering) and the
//! relocation codes from Arm's "ELF for the Arm 64-bit Architecture" (AAELF64), section
//! "Relocation codes":
//!
//! - `e_shnum` 0 with section headers present means the count is section 0's `sh_size`;
//!   `e_shstrndx` `SHN_XINDEX` means the index is section 0's `sh_link` ("Extended Section
//!   Numbering"); a symbol whose `st_shndx` is `SHN_XINDEX` takes its index from the
//!   `SHT_SYMTAB_SHNDX` section linked to the symbol table;
//! - in a relocatable file "st_value holds a section offset for a defined symbol", and for
//!   `SHN_COMMON` "st_value holds alignment constraints";
//! - `SHT_GROUP`'s contents are a flag word (`GRP_COMDAT` = 1) then section header indexes; its
//!   `sh_link` is the symbol table and `sh_info` the symbol whose name is the signature;
//! - RELA only: AArch64 objects use RELA ("A RELA format relocation must be used"); `SHT_REL`
//!   is refused by name.
//!
//! Section kinds go by name (`.text`, `.rodata`, `.data`, `.bss`, `.tdata`, `.tbss`,
//! `.init_array`, `.fini_array`, `.eh_frame`, `.gcc_except_table`, each also with a `.suffix`),
//! after two rules of the format itself: a section without `SHF_ALLOC` never occupies memory in
//! the process ("The section occupies memory during process execution" is the flag's meaning),
//! so it is [`SectionKind::Skip`]; an `SHT_NOBITS` section occupies no file space, so it is
//! zero-fill (thread-local with `SHF_TLS`). An allocated section of any other name is
//! [`SectionKind::Other`].

use blaze_own::{AVec, Alloc, Vec};

use crate::Platform;
use crate::object::{
    Bind, Def, Group, Object, ReadError, Reloc, RelocKind, RelocTo, Section, SectionKind, Symbol, at32, finish_relocs, fits, index, name_at, offset,
    place_fits, push, slice, u8_at, u16_at, u32_at, u64_at,
};

/// `ELFMAG`.
pub(crate) const MAGIC_BYTES: [u8; 4] = [0x7f, b'E', b'L', b'F'];

const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const EV_CURRENT: u8 = 1;
const ET_REL: u16 = 1;
const EM_AARCH64: u16 = 183;

const SHDR_LEN: usize = 64;
const SYM_LEN: usize = 24;
const RELA_LEN: usize = 24;

const SHT_NULL: u32 = 0;
const SHT_SYMTAB: u32 = 2;
const SHT_STRTAB: u32 = 3;
const SHT_RELA: u32 = 4;
const SHT_HASH: u32 = 5;
const SHT_DYNAMIC: u32 = 6;
const SHT_NOBITS: u32 = 8;
const SHT_REL: u32 = 9;
const SHT_SHLIB: u32 = 10;
const SHT_DYNSYM: u32 = 11;
const SHT_INIT_ARRAY: u32 = 14;
const SHT_FINI_ARRAY: u32 = 15;
const SHT_GROUP: u32 = 17;
const SHT_SYMTAB_SHNDX: u32 = 18;

const SHF_ALLOC: u64 = 0x2;
const SHF_TLS: u64 = 0x400;
const SHF_COMPRESSED: u64 = 0x800;

const GRP_COMDAT: u32 = 0x1;

const SHN_UNDEF: u16 = 0;
const SHN_LORESERVE: u16 = 0xff00;
const SHN_ABS: u16 = 0xfff1;
const SHN_COMMON: u16 = 0xfff2;
const SHN_XINDEX: u16 = 0xffff;

const STB_LOCAL: u8 = 0;
const STB_GLOBAL: u8 = 1;
const STB_WEAK: u8 = 2;
const STT_SECTION: u8 = 3;
const STT_GNU_IFUNC: u8 = 10;
const STV_INTERNAL: u8 = 1;
const STV_HIDDEN: u8 = 2;

const R_AARCH64_ABS64: u32 = 257;
const R_AARCH64_ABS32: u32 = 258;
const R_AARCH64_PREL64: u32 = 260;
const R_AARCH64_PREL32: u32 = 261;
const R_AARCH64_ADR_PREL_PG_HI21: u32 = 275;
const R_AARCH64_ADD_ABS_LO12_NC: u32 = 277;
const R_AARCH64_LDST8_ABS_LO12_NC: u32 = 278;
const R_AARCH64_TSTBR14: u32 = 279;
const R_AARCH64_CONDBR19: u32 = 280;
const R_AARCH64_JUMP26: u32 = 282;
const R_AARCH64_CALL26: u32 = 283;
const R_AARCH64_LDST16_ABS_LO12_NC: u32 = 284;
const R_AARCH64_LDST32_ABS_LO12_NC: u32 = 285;
const R_AARCH64_LDST64_ABS_LO12_NC: u32 = 286;
const R_AARCH64_LDST128_ABS_LO12_NC: u32 = 299;
const R_AARCH64_ADR_GOT_PAGE: u32 = 311;
const R_AARCH64_LD64_GOT_LO12_NC: u32 = 312;
const R_AARCH64_TLSIE_ADR_GOTTPREL_PAGE21: u32 = 541;
const R_AARCH64_TLSIE_LD64_GOTTPREL_LO12_NC: u32 = 542;
const R_AARCH64_TLSLE_ADD_TPREL_HI12: u32 = 549;
const R_AARCH64_TLSLE_ADD_TPREL_LO12_NC: u32 = 551;
const R_AARCH64_TLSDESC_ADR_PAGE21: u32 = 562;
const R_AARCH64_TLSDESC_LD64_LO12: u32 = 563;
const R_AARCH64_TLSDESC_ADD_LO12: u32 = 564;
const R_AARCH64_TLSDESC_CALL: u32 = 569;

/// One `Elf64_Shdr`, the fields the reader uses, with where it stands.
#[derive(Clone, Copy)]
struct Shdr {
    name: u32,
    kind: u32,
    flags: u64,
    offset: u64,
    size: u64,
    link: u32,
    info: u32,
    align: u64,
    at: usize,
}

/// Reads one ELF64 AArch64 relocatable object in place.
pub fn read<'a>(bytes: &'a [u8], alloc: Alloc<'a>) -> Result<Object<'a>, ReadError> {
    fits(bytes)?;
    if slice(bytes, 0, 4)? != MAGIC_BYTES {
        return Err(ReadError::WrongFormat { want: Platform::LinuxArm64 });
    }
    let class = u8_at(bytes, 4)?;
    if class != ELFCLASS64 {
        return Err(ReadError::Unsupported { what: "elf class (ELFCLASS64 only)", value: u32::from(class), at: 4 });
    }
    let data = u8_at(bytes, 5)?;
    if data != ELFDATA2LSB {
        return Err(ReadError::Unsupported { what: "elf data encoding (ELFDATA2LSB only)", value: u32::from(data), at: 5 });
    }
    let version = u8_at(bytes, 6)?;
    if version != EV_CURRENT {
        return Err(ReadError::Unsupported { what: "elf ident version (EV_CURRENT only)", value: u32::from(version), at: 6 });
    }
    let e_type = u16_at(bytes, 16)?;
    if e_type != ET_REL {
        return Err(ReadError::Unsupported { what: "elf type (ET_REL only)", value: u32::from(e_type), at: 16 });
    }
    let machine = u16_at(bytes, 18)?;
    if machine != EM_AARCH64 {
        return Err(ReadError::Unsupported { what: "elf machine (EM_AARCH64 only)", value: u32::from(machine), at: 18 });
    }
    let shoff = index(u64_at(bytes, 40)?, 40)?;
    let shentsize = u16_at(bytes, 58)?;
    let e_shnum = u16_at(bytes, 60)?;
    let e_shstrndx = u16_at(bytes, 62)?;
    if shoff == 0 {
        return Err(ReadError::Malformed { what: "an elf object without section headers (e_shoff 0)", at: 40 });
    }
    if usize::from(shentsize) != SHDR_LEN {
        return Err(ReadError::Malformed { what: "e_shentsize is not sizeof(Elf64_Shdr)", at: 58 });
    }
    let first = shdr_at(bytes, shoff)?;
    let shnum = if e_shnum == 0 { index(first.size, shoff + 32)? } else { usize::from(e_shnum) };
    let shstrndx = if e_shstrndx == SHN_XINDEX { first.link as usize } else { usize::from(e_shstrndx) };
    slice(bytes, shoff, offset(0, shnum, SHDR_LEN, 60)?)?;

    let mut headers: AVec<'a, Shdr> = Vec::try_with_capacity_in(shnum, alloc).map_err(|_| ReadError::Alloc)?;
    for n in 0..shnum {
        push(&mut headers, shdr_at(bytes, shoff + n * SHDR_LEN)?)?;
    }
    let names_header = *headers.get(shstrndx).ok_or(ReadError::Malformed { what: "e_shstrndx names no section", at: 62 })?;
    if names_header.kind != SHT_STRTAB {
        return Err(ReadError::Malformed { what: "e_shstrndx's section is not SHT_STRTAB", at: at32(names_header.at) });
    }
    let names = contents(bytes, &names_header)?;

    let mut sections: AVec<'a, Section<'a>> = Vec::try_with_capacity_in(shnum, alloc).map_err(|_| ReadError::Alloc)?;
    let mut symtab: Option<usize> = None;
    for (n, h) in headers.iter().enumerate() {
        let name = name_at(names, h.name, h.at)?;
        let kind = kind_of(name, h)?;
        let align_log2 = match h.align {
            0 | 1 => 0,
            a if a.is_power_of_two() => a.trailing_zeros() as u8,
            _ => return Err(ReadError::Malformed { what: "an elf section's sh_addralign is not a power of two", at: at32(h.at + 48) }),
        };
        let section_bytes = if h.kind == SHT_NOBITS || h.kind == SHT_NULL { &[][..] } else { contents(bytes, h)? };
        if h.kind == SHT_SYMTAB {
            if symtab.is_some() {
                return Err(ReadError::Malformed { what: "a second SHT_SYMTAB in a relocatable object", at: at32(h.at) });
            }
            symtab = Some(n);
        }
        push(&mut sections, Section { segment: &[], name, kind, align_log2, bytes: section_bytes, size: h.size, relocs: 0..0 })?;
    }

    // The symbols, and for each the section an `STT_SECTION` symbol stands for.
    let mut symbols: AVec<'a, Symbol<'a>> = Vec::new_in(alloc);
    let mut section_of: AVec<'a, Option<u32>> = Vec::new_in(alloc);
    if let Some(table) = symtab {
        let h = headers[table];
        let strings_header = *headers.get(h.link as usize).ok_or(ReadError::Malformed { what: "SHT_SYMTAB's sh_link names no section", at: at32(h.at + 40) })?;
        if strings_header.kind != SHT_STRTAB {
            return Err(ReadError::Malformed { what: "SHT_SYMTAB's sh_link is not SHT_STRTAB", at: at32(h.at + 40) });
        }
        let strings = contents(bytes, &strings_header)?;
        let entries = contents(bytes, &h)?;
        if entries.len() % SYM_LEN != 0 {
            return Err(ReadError::Malformed { what: "SHT_SYMTAB's size is not a multiple of sizeof(Elf64_Sym)", at: at32(h.at + 32) });
        }
        let extended = headers.iter().find(|x| x.kind == SHT_SYMTAB_SHNDX && x.link as usize == table).copied();
        let extended = match extended {
            Some(x) => Some(contents(bytes, &x)?),
            None => None,
        };
        for n in 0..entries.len() / SYM_LEN {
            let (symbol, section) = read_symbol(bytes, h.offset as usize + n * SYM_LEN, n, strings, extended, sections.len())?;
            push(&mut symbols, symbol)?;
            push(&mut section_of, section)?;
        }
    }

    let mut groups: AVec<'a, Group<'a>> = Vec::new_in(alloc);
    let mut relocs: AVec<'a, Reloc> = Vec::new_in(alloc);
    for h in headers.iter() {
        match h.kind {
            SHT_GROUP => {
                let group = read_group(bytes, h, symtab, &symbols, &section_of, &sections, alloc)?;
                push(&mut groups, group)?;
            }
            SHT_RELA => read_relas(bytes, h, symtab, &symbols, &section_of, &sections, &mut relocs)?,
            _ => {}
        }
    }
    finish_relocs(&mut relocs, &mut sections)?;
    Ok(Object { sections, symbols, relocs, unwind: Vec::new_in(alloc), groups })
}

fn shdr_at(bytes: &[u8], at: usize) -> Result<Shdr, ReadError> {
    Ok(Shdr {
        name: u32_at(bytes, at)?,
        kind: u32_at(bytes, at + 4)?,
        flags: u64_at(bytes, at + 8)?,
        offset: u64_at(bytes, at + 24)?,
        size: u64_at(bytes, at + 32)?,
        link: u32_at(bytes, at + 40)?,
        info: u32_at(bytes, at + 44)?,
        align: u64_at(bytes, at + 48)?,
        at,
    })
}

/// A section's bytes in the file, bounds-checked.
fn contents<'a>(bytes: &'a [u8], h: &Shdr) -> Result<&'a [u8], ReadError> {
    slice(bytes, index(h.offset, h.at + 24)?, index(h.size, h.at + 32)?)
}

/// `name` is `prefix` or `prefix` followed by a `.suffix`.
fn named(name: &[u8], prefix: &[u8]) -> bool {
    name == prefix || (name.starts_with(prefix) && name.get(prefix.len()) == Some(&b'.'))
}

const BY_NAME: [(&[u8], SectionKind); 10] = [
    (b".text", SectionKind::Code),
    (b".rodata", SectionKind::ReadOnly),
    (b".data", SectionKind::Data),
    (b".bss", SectionKind::ZeroFill),
    (b".tdata", SectionKind::ThreadData),
    (b".tbss", SectionKind::ThreadZero),
    (b".init_array", SectionKind::InitPointers),
    (b".fini_array", SectionKind::TermPointers),
    (b".eh_frame", SectionKind::EhFrame),
    (b".gcc_except_table", SectionKind::ExceptTable),
];

fn kind_of(name: &[u8], h: &Shdr) -> Result<SectionKind, ReadError> {
    match h.kind {
        SHT_NULL | SHT_SYMTAB | SHT_STRTAB | SHT_RELA | SHT_GROUP | SHT_SYMTAB_SHNDX => return Ok(SectionKind::Skip),
        SHT_REL => return Err(ReadError::Unsupported { what: "elf SHT_REL relocations (RELA only)", value: SHT_REL, at: at32(h.at + 4) }),
        SHT_HASH | SHT_DYNAMIC | SHT_SHLIB | SHT_DYNSYM => {
            return Err(ReadError::Unsupported { what: "elf section type of a linked file, in a relocatable object", value: h.kind, at: at32(h.at + 4) });
        }
        _ => {}
    }
    if name == b".note.GNU-stack" || name == b".comment" || name.starts_with(b".debug_") || name == b".llvm_addrsig" {
        return Ok(SectionKind::Skip);
    }
    if h.flags & SHF_ALLOC == 0 {
        return Ok(SectionKind::Skip);
    }
    if h.flags & SHF_COMPRESSED != 0 {
        return Err(ReadError::Unsupported { what: "elf SHF_COMPRESSED on an allocated section", value: h.kind, at: at32(h.at + 8) });
    }
    if h.kind == SHT_NOBITS {
        return Ok(if h.flags & SHF_TLS != 0 { SectionKind::ThreadZero } else { SectionKind::ZeroFill });
    }
    for (prefix, kind) in BY_NAME {
        if named(name, prefix) {
            if matches!(kind, SectionKind::ZeroFill | SectionKind::ThreadZero) {
                return Err(ReadError::Malformed { what: "a .bss or .tbss section that is not SHT_NOBITS", at: at32(h.at + 4) });
            }
            return Ok(kind);
        }
    }
    Ok(match h.kind {
        SHT_INIT_ARRAY => SectionKind::InitPointers,
        SHT_FINI_ARRAY => SectionKind::TermPointers,
        _ => SectionKind::Other,
    })
}

/// Symbol `n`, at `at`; and the section it stands for when it is an `STT_SECTION` symbol.
fn read_symbol<'a>(bytes: &'a [u8], at: usize, n: usize, strings: &'a [u8], extended: Option<&[u8]>, nsections: usize) -> Result<(Symbol<'a>, Option<u32>), ReadError> {
    let st_name = u32_at(bytes, at)?;
    let info = u8_at(bytes, at + 4)?;
    let other = u8_at(bytes, at + 5)?;
    let shndx = u16_at(bytes, at + 6)?;
    let value = u64_at(bytes, at + 8)?;
    let size = u64_at(bytes, at + 16)?;
    let name = name_at(strings, st_name, at)?;
    let binding = info >> 4;
    let typ = info & 0xf;
    if typ == STT_GNU_IFUNC {
        return Err(ReadError::Unsupported { what: "elf STT_GNU_IFUNC symbol", value: u32::from(typ), at: at32(at + 4) });
    }
    let section_index = |number: u32| -> Result<u32, ReadError> {
        if number as usize >= nsections {
            return Err(ReadError::Malformed { what: "an elf symbol's section index names no section", at: at32(at + 6) });
        }
        Ok(number)
    };
    let def = match shndx {
        SHN_UNDEF => Def::Undefined,
        SHN_ABS => Def::Absolute(value),
        SHN_COMMON => {
            let align_log2 = match value {
                0 | 1 => 0,
                a if a.is_power_of_two() => a.trailing_zeros() as u8,
                _ => return Err(ReadError::Malformed { what: "an SHN_COMMON symbol's alignment (st_value) is not a power of two", at: at32(at + 8) }),
            };
            Def::Common { size, align_log2 }
        }
        SHN_XINDEX => {
            let table = extended.ok_or(ReadError::Malformed { what: "an SHN_XINDEX symbol with no SHT_SYMTAB_SHNDX section", at: at32(at + 6) })?;
            let number = u32_at(table, n * 4)?;
            Def::In { section: section_index(number)?, offset: value }
        }
        reserved if reserved >= SHN_LORESERVE => {
            return Err(ReadError::Unsupported { what: "elf reserved section index", value: u32::from(reserved), at: at32(at + 6) });
        }
        number => Def::In { section: section_index(u32::from(number))?, offset: value },
    };
    let bind = match (binding, def) {
        (STB_LOCAL, _) => Bind::Local,
        (STB_GLOBAL, _) => Bind::Global,
        (STB_WEAK, Def::Undefined) => Bind::WeakUndefined,
        (STB_WEAK, _) => Bind::Weak,
        (other, _) => return Err(ReadError::Unsupported { what: "elf symbol binding (STB_LOCAL, STB_GLOBAL, STB_WEAK only)", value: u32::from(other), at: at32(at + 4) }),
    };
    let hidden = matches!(other & 0x3, STV_HIDDEN | STV_INTERNAL);
    let section = if typ == STT_SECTION {
        match def {
            Def::In { section, .. } => Some(section),
            _ => return Err(ReadError::Malformed { what: "an STT_SECTION symbol not defined in a section", at: at32(at + 6) }),
        }
    } else {
        None
    };
    Ok((Symbol { name, def, bind, hidden }, section))
}

fn read_group<'a>(
    bytes: &'a [u8],
    h: &Shdr,
    symtab: Option<usize>,
    symbols: &[Symbol<'a>],
    section_of: &[Option<u32>],
    sections: &[Section<'a>],
    alloc: Alloc<'a>,
) -> Result<Group<'a>, ReadError> {
    if Some(h.link as usize) != symtab {
        return Err(ReadError::Malformed { what: "SHT_GROUP's sh_link is not the symbol table", at: at32(h.at + 40) });
    }
    let words = contents(bytes, h)?;
    if words.len() < 4 || words.len() % 4 != 0 {
        return Err(ReadError::Malformed { what: "SHT_GROUP's size is not a flag word and whole section indexes", at: at32(h.at + 32) });
    }
    let flags = u32_at(words, 0)?;
    if flags != GRP_COMDAT {
        return Err(ReadError::Unsupported { what: "elf SHT_GROUP flags (GRP_COMDAT only)", value: flags, at: at32(h.at + 24) });
    }
    let symbol = h.info as usize;
    let signature = match (section_of.get(symbol), symbols.get(symbol)) {
        (Some(Some(section)), _) => sections[*section as usize].name,
        (_, Some(s)) => s.name,
        _ => return Err(ReadError::Malformed { what: "SHT_GROUP's sh_info names no symbol", at: at32(h.at + 44) }),
    };
    let mut members: AVec<'a, u32> = Vec::new_in(alloc);
    for n in 1..words.len() / 4 {
        let number = u32_at(words, n * 4)?;
        if number as usize >= sections.len() {
            return Err(ReadError::Malformed { what: "an SHT_GROUP member names no section", at: at32(h.at + 24) });
        }
        push(&mut members, number)?;
    }
    Ok(Group { signature, sections: members })
}

fn reloc_kind(code: u32) -> Option<RelocKind> {
    Some(match code {
        R_AARCH64_ABS64 => RelocKind::Abs64,
        R_AARCH64_ABS32 => RelocKind::Abs32,
        R_AARCH64_PREL32 => RelocKind::Rel32,
        R_AARCH64_PREL64 => RelocKind::Rel64,
        R_AARCH64_CALL26 | R_AARCH64_JUMP26 => RelocKind::Branch26,
        R_AARCH64_CONDBR19 => RelocKind::CondBranch19,
        R_AARCH64_TSTBR14 => RelocKind::TestBranch14,
        R_AARCH64_ADR_PREL_PG_HI21 => RelocKind::Page21,
        R_AARCH64_ADD_ABS_LO12_NC | R_AARCH64_LDST8_ABS_LO12_NC => RelocKind::PageOff12 { scale_log2: 0 },
        R_AARCH64_LDST16_ABS_LO12_NC => RelocKind::PageOff12 { scale_log2: 1 },
        R_AARCH64_LDST32_ABS_LO12_NC => RelocKind::PageOff12 { scale_log2: 2 },
        R_AARCH64_LDST64_ABS_LO12_NC => RelocKind::PageOff12 { scale_log2: 3 },
        R_AARCH64_LDST128_ABS_LO12_NC => RelocKind::PageOff12 { scale_log2: 4 },
        R_AARCH64_ADR_GOT_PAGE => RelocKind::GotPage21,
        R_AARCH64_LD64_GOT_LO12_NC => RelocKind::GotPageOff12,
        R_AARCH64_TLSDESC_ADR_PAGE21 => RelocKind::TlsDescPage21,
        R_AARCH64_TLSDESC_LD64_LO12 => RelocKind::TlsDescLo12,
        R_AARCH64_TLSDESC_ADD_LO12 => RelocKind::TlsDescAdd,
        R_AARCH64_TLSDESC_CALL => RelocKind::TlsDescCall,
        R_AARCH64_TLSIE_ADR_GOTTPREL_PAGE21 => RelocKind::TlsIePage21,
        R_AARCH64_TLSIE_LD64_GOTTPREL_LO12_NC => RelocKind::TlsIeLo12,
        R_AARCH64_TLSLE_ADD_TPREL_HI12 => RelocKind::TlsLeHi12,
        R_AARCH64_TLSLE_ADD_TPREL_LO12_NC => RelocKind::TlsLeLo12,
        _ => return None,
    })
}

/// One `SHT_RELA` section's entries, unless its target section is [`SectionKind::Skip`].
fn read_relas(bytes: &[u8], h: &Shdr, symtab: Option<usize>, symbols: &[Symbol<'_>], section_of: &[Option<u32>], sections: &[Section<'_>], relocs: &mut AVec<'_, Reloc>) -> Result<(), ReadError> {
    let target = h.info as usize;
    let section = sections.get(target).ok_or(ReadError::Malformed { what: "SHT_RELA's sh_info names no section", at: at32(h.at + 44) })?;
    if section.kind == SectionKind::Skip {
        return Ok(());
    }
    if Some(h.link as usize) != symtab {
        return Err(ReadError::Malformed { what: "SHT_RELA's sh_link is not the symbol table", at: at32(h.at + 40) });
    }
    let entries = contents(bytes, h)?;
    if entries.len() % RELA_LEN != 0 {
        return Err(ReadError::Malformed { what: "SHT_RELA's size is not a multiple of sizeof(Elf64_Rela)", at: at32(h.at + 32) });
    }
    let base = h.offset as usize;
    for n in 0..entries.len() / RELA_LEN {
        let at = base + n * RELA_LEN;
        let r_offset = u64_at(bytes, at)?;
        let r_info = u64_at(bytes, at + 8)?;
        let addend = u64_at(bytes, at + 16)? as i64;
        let code = (r_info & 0xffff_ffff) as u32;
        let symbol = (r_info >> 32) as usize;
        let kind = reloc_kind(code).ok_or(ReadError::Unsupported { what: "elf reloc", value: code, at: at32(at + 8) })?;
        if symbol == 0 || symbol >= symbols.len() {
            return Err(ReadError::Malformed { what: "an elf relocation's symbol index is 0 or names no symbol", at: at32(at + 8) });
        }
        let offset = place_fits(r_offset, kind, section.size, at)?;
        let to = match section_of[symbol] {
            Some(s) => RelocTo::Section(s),
            None => RelocTo::Symbol(symbol as u32),
        };
        push(relocs, Reloc { section: target as u32, offset, kind, to, addend })?;
    }
    Ok(())
}
