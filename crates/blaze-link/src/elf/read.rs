//! Reading ELF native inputs: relocatable objects, `ar` archives, shared objects, and the GNU
//! linker scripts glibc ships as `libc.so`.
//!
//! Sections and symbols of an object keep their ELF indexes, so a symbol's section, a group's
//! members and a relocation's symbol are the ELF numbers unchanged.
//!
//! Nothing is copied but names: a section's bytes are its range of the input, and each string
//! of a string table is pushed once into the file's [`Names`] ([`strings`]).

mod archive;
mod script;
mod shared;
mod strings;
#[cfg(test)]
mod tests;

use std::collections::hash_map::Entry;
use std::fmt;

use blaze_codegen::Arch;
use blaze_hash::HashMap;
use object::Endianness;
use object::SectionIndex;
use object::elf;
use object::read::elf::{FileHeader as _, Rela as _, SectionHeader as _, SectionTable, Sym as _};

use self::strings::Table;
use crate::NativeKind;
use crate::model::{
    Apply, Binding, Data, Group, Name, Names, NativeFile, NativeFixup, Object, Section,
    SectionKind, Symbol, SymbolDef, SymbolKind, Tie,
};

type Header = elf::FileHeader64<Endianness>;
type Sym = elf::Sym64<Endianness>;

/// Every ELF input blaze links is little-endian (x86-64, AArch64); [`header`] checks it.
const ENDIAN: Endianness = Endianness::Little;

/// The symbol `R_X86_64_GOTPC*` and `GOTOFF64` are against.
const GOT: &str = "_GLOBAL_OFFSET_TABLE_";

/// Reads `bytes`, the input `name` the link line takes as `kind`. `object` reads its structs in
/// place at any alignment, so the input is read where it lies (an archive member is only
/// 2-aligned).
pub(super) fn read(
    arch: Arch,
    name: &str,
    bytes: &[u8],
    kind: &NativeKind,
) -> Result<NativeFile, String> {
    if bytes.starts_with(&object::archive::THIN_MAGIC) {
        // alloc: error path
        return Err(format!(
            "{name}: a thin archive, whose members are files outside it: blaze links only \
             archives that hold their members"
        ));
    }
    if bytes.starts_with(&object::archive::MAGIC) {
        return match kind {
            NativeKind::Archive { .. } => archive::read(arch, name, bytes).map(NativeFile::Archive),
            _ => Err(mismatch(name, "an archive", kind)),
        };
    }
    if bytes.starts_with(&elf::ELFMAG) {
        let header = header(arch, name, bytes)?;
        return match (header.e_type(ENDIAN), kind) {
            (elf::ET_REL, NativeKind::Object) => object(arch, name, bytes).map(NativeFile::Object),
            (elf::ET_DYN, NativeKind::Shared { .. }) => {
                shared::read(name, bytes).map(NativeFile::Shared)
            }
            (elf::ET_REL, _) => Err(mismatch(name, "a relocatable object (ET_REL)", kind)),
            (elf::ET_DYN, _) => Err(mismatch(name, "a shared object (ET_DYN)", kind)),
            // alloc: error path
            (e_type, _) => Err(format!(
                "{name}: an ELF file of type {e_type}, neither a relocatable object (ET_REL) \
                 nor a shared object (ET_DYN)"
            )),
        };
    }
    match kind {
        NativeKind::Shared { .. } => script::read(name, bytes).map(NativeFile::Shared),
        _ => Err(format!("{name}: neither an ELF file nor an archive, given as {}", taken(kind))), // alloc: error path
    }
}

// Reading in place at any alignment needs `object`'s structs to be 1-aligned.
const _: () = assert!(align_of::<Header>() == 1 && align_of::<elf::Rela64<Endianness>>() == 1);

fn taken(kind: &NativeKind) -> &'static str {
    match kind {
        NativeKind::Object => "an object",
        NativeKind::Archive { .. } => "an archive",
        NativeKind::Shared { .. } => "a shared library",
    }
}

/// The error of an input the link line gives as something it is not.
fn mismatch(name: &str, what: &str, kind: &NativeKind) -> String {
    format!("{name}: {what}, but the link line gives it as {}", taken(kind)) // alloc: error path
}

/// The ELF header of `bytes`, checked to be a 64-bit little-endian file of the output's machine.
/// `name` is formatted only into an error.
fn header<'data>(
    arch: Arch,
    name: impl fmt::Display + Copy,
    bytes: &'data [u8],
) -> Result<&'data Header, String> {
    let header =
        Header::parse(bytes).map_err(|error| format!("{name}: not a 64-bit ELF file: {error}"))?; // alloc: error path
    if !header.is_little_endian() {
        return Err(format!("{name}: a big-endian ELF file")); // alloc: error path
    }
    let (machine, label) = match arch {
        Arch::X86_64 => (elf::EM_X86_64, "x86-64"),
        Arch::Aarch64 => (elf::EM_AARCH64, "AArch64"),
        Arch::Wasm32 => return Err(format!("{name}: an ELF input to a wasm32 link")), // alloc: error path
    };
    let found = header.e_machine(ENDIAN);
    if found != machine {
        // alloc: error path
        return Err(format!(
            "{name}: an ELF file for machine {found} ({}), but the output is {label}",
            machine_name(found)
        ));
    }
    Ok(header)
}

fn machine_name(machine: elf::Machine) -> &'static str {
    match machine {
        elf::EM_X86_64 => "x86-64",
        elf::EM_AARCH64 => "AArch64",
        elf::EM_386 => "i386",
        elf::EM_ARM => "ARM",
        elf::EM_RISCV => "RISC-V",
        _ => "another machine",
    }
}

fn utf8<'a>(name: &str, what: &str, bytes: &'a [u8]) -> Result<&'a str, String> {
    std::str::from_utf8(bytes).map_err(|_| {
        format!("{name}: {what} {} is not UTF-8", String::from_utf8_lossy(bytes)) // alloc: error path
    })
}

/// An error of a file's [`Names`] (past 4 GiB), naming the input.
fn full(name: &str) -> impl Fn(String) -> String + Copy + '_ {
    move |why| format!("{name}: {why}") // alloc: error path
}

/// String table `index` of the input `name`, `data`.
fn table<'data>(
    name: &str,
    sections: &SectionTable<'data, Header>,
    index: SectionIndex,
    data: &'data [u8],
) -> Result<Table<'data>, String> {
    let bytes = sections
        .section(index)
        .and_then(|it| it.data(ENDIAN, data))
        .map_err(|error| format!("{name}: string table {}: {error}", index.0))?; // alloc: error path
    Table::new(name, bytes)
}

/// The length of a column, as the end of a run of it.
fn end(name: &str, len: usize, what: &str) -> Result<u32, String> {
    u32::try_from(len).map_err(|_| format!("{name}: more than 2^32 {what} in one object")) // alloc: error path
}

/// The last component of a path: what a shared library without a `DT_SONAME`, or a linker
/// script, is known by.
fn file_name(path: &str) -> String {
    path.rsplit_once('/').map_or(path, |(_, file)| file).to_owned() // alloc: once per file, `Shared::name`
}

/// A symbol's kind; an IFUNC is a function here, and [`object`] refuses one. A type ELF does
/// not define (an OS or processor one other than IFUNC) is refused: what it asks of the link is
/// unknown.
fn kind(name: &str, symbol: impl FnOnce() -> String, st_type: elf::SymbolType) -> Result<SymbolKind, String> {
    Ok(match st_type {
        elf::STT_FUNC | elf::STT_GNU_IFUNC => SymbolKind::Function,
        elf::STT_OBJECT | elf::STT_COMMON => SymbolKind::Data,
        elf::STT_TLS => SymbolKind::Tls,
        elf::STT_SECTION => SymbolKind::Section,
        elf::STT_NOTYPE | elf::STT_FILE => SymbolKind::Other,
        // alloc: error path
        other => return Err(format!("{name}: symbol {} has type {}, which blaze-link does not read", symbol(), other.0)),
    })
}

/// The defined global and weak symbols of the relocatable object `data`, in symbol table
/// order, as offsets of their names in its string table (the first part): what an archive
/// without a symbol table is indexed by.
fn defined<'data>(
    arch: Arch,
    name: &str,
    data: &'data [u8],
) -> Result<(&'data [u8], Vec<u32>), String> {
    if !data.starts_with(&elf::ELFMAG) {
        // alloc: error path
        return Err(format!("{name}: an archive member that is not an ELF object"));
    }
    let header = header(arch, name, data)?;
    if header.e_type(ENDIAN) != elf::ET_REL {
        // alloc: error path
        return Err(format!("{name}: an archive member that is not a relocatable object (ET_REL)"));
    }
    // alloc: error path
    let error = |error: object::read::Error| format!("{name}: {error}");
    let sections = header.sections(ENDIAN, data).map_err(error)?;
    let symtab = sections.symbols(ENDIAN, data, elf::SHT_SYMTAB).map_err(error)?;
    let strtab = match symtab.len() {
        0 => &[][..],
        _ => sections
            .section(symtab.string_section())
            .and_then(|it| it.data(ENDIAN, data))
            .map_err(error)?,
    };
    let mut offsets = Vec::with_capacity(symtab.len());
    for (index, symbol) in symtab.enumerate() {
        if index.0 == 0 || symbol.st_shndx(ENDIAN) == elf::SHN_UNDEF {
            continue;
        }
        if matches!(symbol.st_bind(), elf::STB_GLOBAL | elf::STB_WEAK | elf::STB_GNU_UNIQUE) {
            offsets.push(symbol.st_name(ENDIAN));
        }
    }
    Ok((strtab, offsets))
}

/// Reads a relocatable object.
fn object(arch: Arch, name: &str, data: &[u8]) -> Result<Object, String> {
    let header = header(arch, name, data)?;
    // alloc: error path
    let error = |error: object::read::Error| format!("{name}: {error}");
    let full = full(name);
    let sections = header.sections(ENDIAN, data).map_err(error)?;
    let symtab = sections.symbols(ENDIAN, data, elf::SHT_SYMTAB).map_err(error)?;

    // Every column is sized once, from the section headers: the arena by the string tables its
    // names come from, the fixups by the relocation sections, the groups by the group
    // sections. Each is bounded by the input, whatever the headers claim.
    let (mut name_bytes, mut relas, mut relocation_sections) = (GOT.len() as u64, 0u64, 0usize);
    let (mut groups, mut group_words) = (0usize, 0u64);
    for section in sections.iter() {
        match section.sh_type(ENDIAN) {
            elf::SHT_STRTAB => name_bytes = name_bytes.saturating_add(section.sh_size(ENDIAN)),
            elf::SHT_RELA => {
                relas = relas.saturating_add(section.sh_size(ENDIAN));
                relocation_sections += 1;
            }
            elf::SHT_REL | elf::SHT_CREL => relocation_sections += 1,
            elf::SHT_GROUP => {
                groups += 1;
                group_words = group_words.saturating_add(section.sh_size(ENDIAN) / 4);
            }
            _ => {}
        }
    }
    let input = data.len() as u64;
    let rela_size = size_of::<elf::Rela64<Endianness>>() as u64;
    let mut object = Object {
        sections: Vec::with_capacity(sections.len()),
        symbols: Vec::with_capacity(symtab.len()),
        fixups: Vec::with_capacity((relas.min(input) / rela_size) as usize),
        groups: Vec::with_capacity(groups),
        grouped: Vec::with_capacity(group_words.min(input / 4) as usize),
        names: Names::with_capacity(name_bytes.min(input) as usize),
        ..Object::default()
    };
    let mut shstrtab = match sections.len() {
        0 => Table::new(name, &[])?,
        _ => table(name, &sections, header.section_strings_index(ENDIAN, data).map_err(error)?, data)?,
    };
    for (index, section) in sections.enumerate() {
        let section =
            self::section(arch, name, &mut shstrtab, index.0, section, data, &mut object.names)?;
        object.sections.push(section);
    }
    // lld's `SHF_LINK_ORDER`: a loaded section is tied to the one its `sh_link` names; with
    // `sh_link` 0 it is read as if without the flag, as lld does.
    for (index, header) in sections.enumerate() {
        let to = header.sh_link(ENDIAN) as usize;
        if !header.sh_flags(ENDIAN).contains(elf::SHF_LINK_ORDER)
            || to == 0
            || object.sections[index.0].what == SectionKind::Dropped
        {
            continue;
        }
        if object.sections.get(to).is_none_or(|it| it.what == SectionKind::Dropped) || to == index.0 {
            // alloc: error path
            return Err(format!(
                "{name}: section {} is SHF_LINK_ORDER to section {to}, which is not another section the link loads",
                object.name(object.sections[index.0].name)
            ));
        }
        object.ties.push(Tie { section: index.0 as u32, to: to as u32, ordered: true });
    }

    let mut strtab = match symtab.len() {
        0 => Table::new(name, &[])?,
        _ => table(name, &sections, symtab.string_section(), data)?,
    };
    // The null symbol, and the ones relocations add, are unnamed: one empty name for all.
    let empty = object.names.push("").map_err(full)?;
    for (index, symbol) in symtab.enumerate() {
        if index.0 == 0 {
            object.symbols.push(Symbol {
                name: empty,
                def: SymbolDef::Absolute(0),
                binding: Binding::Local,
                kind: SymbolKind::Other,
                hidden: false,
            });
            continue;
        }
        let interned =
            strtab.name(&mut object.names, name, "the symbol name", symbol.st_name(ENDIAN))?;
        let symbol_name = object.names.at(interned);
        let def = match symbol.st_shndx(ENDIAN) {
            elf::SHN_UNDEF => SymbolDef::Undefined,
            elf::SHN_ABS => SymbolDef::Absolute(symbol.st_value(ENDIAN)),
            elf::SHN_COMMON => SymbolDef::Common {
                size: symbol.st_size(ENDIAN),
                align: symbol.st_value(ENDIAN),
            },
            shndx => {
                let section = if shndx == elf::SHN_XINDEX {
                    symtab
                        .symbol_section(ENDIAN, symbol, index)
                        .map_err(error)?
                        .ok_or_else(|| {
                            // alloc: error path
                            format!("{name}: symbol {symbol_name} has no extended section index")
                        })?
                        .0
                } else if shndx.0 < elf::SHN_LORESERVE {
                    usize::from(shndx.0)
                } else {
                    // alloc: error path
                    return Err(format!(
                        "{name}: symbol {symbol_name} is in reserved section {shndx:#x}"
                    ));
                };
                if section >= object.sections.len() {
                    // alloc: error path
                    return Err(format!(
                        "{name}: symbol {symbol_name} is in section {section}, which the object \
                         does not have"
                    ));
                }
                SymbolDef::Section {
                    section: section as u32,
                    value: symbol.st_value(ENDIAN),
                    size: symbol.st_size(ENDIAN),
                }
            }
        };
        let binding = match symbol.st_bind() {
            elf::STB_LOCAL => Binding::Local,
            elf::STB_WEAK => Binding::Weak,
            elf::STB_GLOBAL | elf::STB_GNU_UNIQUE => Binding::Global,
            other => {
                // alloc: error path
                return Err(format!("{name}: symbol {symbol_name} has binding {other}"));
            }
        };
        if symbol.st_type() == elf::STT_GNU_IFUNC {
            // alloc: error path
            return Err(format!(
                "{name}: symbol {symbol_name} is an IFUNC (STT_GNU_IFUNC), which blaze-link does \
                 not link yet: it has no IRELATIVE relocation to resolve one at load"
            ));
        }
        // alloc: error path
        let kind = kind(name, || symbol_name.to_owned(), symbol.st_type())?;
        object.symbols.push(Symbol {
            name: interned,
            def,
            binding,
            kind,
            hidden: matches!(symbol.st_visibility(), elf::STV_HIDDEN | elf::STV_INTERNAL),
        });
    }

    for (index, section) in sections.enumerate() {
        let Some((flag, members)) = section.group(ENDIAN, data).map_err(error)? else {
            continue;
        };
        if !flag.contains(elf::GRP_COMDAT) {
            continue;
        }
        let group = object.sections[index.0].name;
        if section.sh_link(ENDIAN) as usize != symtab.section().0 {
            // alloc: error path
            return Err(format!(
                "{name}: group {} names a symbol table other than .symtab",
                object.name(group)
            ));
        }
        let signature = section.sh_info(ENDIAN) as usize;
        let Some(symbol) = object.symbols.get(signature).filter(|_| signature != 0) else {
            // alloc: error path
            return Err(format!(
                "{name}: group {}'s signature is symbol {signature}, which the object does \
                 not have",
                object.name(group)
            ));
        };
        // The signature's name is already in the arena: the group shares it.
        let group_name = match symbol.def {
            SymbolDef::Section { section, .. } if symbol.kind == SymbolKind::Section => {
                object.sections[section as usize].name
            }
            _ => symbol.name,
        };
        let start = end(name, object.grouped.len(), "group members")?;
        object.grouped.reserve(members.len());
        for member in members {
            let member = member.get(ENDIAN);
            if member as usize >= object.sections.len() || member == 0 {
                // alloc: error path
                return Err(format!(
                    "{name}: group {} names section {member}, which the object does not have",
                    object.name(group_name)
                ));
            }
            object.grouped.push(member);
        }
        let sections = start..end(name, object.grouped.len(), "group members")?;
        object.groups.push(Group { name: group_name, sections, largest: false });
    }

    // Each section's fixups are one run of the column, so relocation sections are read in the
    // order of the section they apply to: the order they nearly always come in, which the
    // sort only checks.
    let mut order: Vec<(u32, u32)> = Vec::with_capacity(relocation_sections);
    order.extend(
        sections
            .enumerate()
            .filter(|(_, it)| matches!(it.sh_type(ENDIAN), elf::SHT_RELA | elf::SHT_REL | elf::SHT_CREL))
            .map(|(index, it)| (it.sh_info(ENDIAN), index.0 as u32)),
    );
    order.sort_unstable();
    let mut added = Added::new(empty);
    // The section the last relocation section applied to, and where its run starts.
    let mut run: Option<(usize, u32)> = None;
    for (target, index) in order {
        let (target, index) = (target as usize, index as usize);
        let section = sections.section(SectionIndex(index)).map_err(error)?;
        let sh_type = section.sh_type(ENDIAN);
        let relocations = object.sections[index].name;
        let Some(applied) = object.sections.get(target).filter(|_| target != 0) else {
            // alloc: error path
            return Err(format!(
                "{name}: relocation section {} applies to section {target}, which the object \
                 does not have",
                object.name(relocations)
            ));
        };
        if applied.what == SectionKind::Dropped {
            continue;
        }
        if sh_type != elf::SHT_RELA {
            // alloc: error path
            return Err(format!(
                "{name}: section {} has {} relocations ({}): x86-64 and AArch64 objects carry \
                 their addends (SHT_RELA)",
                object.name(applied.name),
                if sh_type == elf::SHT_REL { "SHT_REL" } else { "SHT_CREL" },
                object.name(relocations),
            ));
        }
        if section.sh_link(ENDIAN) as usize != symtab.section().0 {
            // alloc: error path
            return Err(format!(
                "{name}: relocation section {} names a symbol table other than .symtab",
                object.name(relocations)
            ));
        }
        let Some((relas, _)) = section.rela(ENDIAN, data).map_err(error)? else {
            continue;
        };
        let start = match run {
            Some((last, start)) if last == target => start,
            _ => end(name, object.fixups.len(), "relocations")?,
        };
        object.fixups.reserve(relas.len());
        for rela in relas {
            let at = rela.r_offset(ENDIAN);
            let r_type = rela.r_type(ENDIAN, false);
            let symbol = rela.r_sym(ENDIAN, false);
            let addend = rela.r_addend(ENDIAN);
            let place = |object: &Object| {
                // alloc: error path
                format!(
                    "{name}: section {}: relocation {} ({}) at {at:#x}",
                    object.name(object.sections[target].name),
                    relocation_name(arch, r_type),
                    r_type.0
                )
            };
            if symbol as usize >= symtab.len() {
                // alloc: error path
                return Err(format!(
                    "{}: symbol {symbol}, which the object does not have",
                    place(&object)
                ));
            }
            let how = match arch {
                Arch::X86_64 => x86_64(r_type),
                Arch::Aarch64 => aarch64(r_type),
                Arch::Wasm32 => Err("an ELF relocation in a wasm32 link"),
            }
            // alloc: error path
            .map_err(|why| format!("{}: {why}", place(&object)))?;
            let make = |apply, symbol, minus| NativeFixup { at, apply, symbol, addend, minus };
            let fixup = match how {
                How::Nothing => continue,
                How::To(apply) => make(apply, symbol, None),
                How::Module(apply) => make(apply, added.module(&mut object.symbols), None),
                How::Size(apply) => {
                    let size = added
                        .size(&mut object.symbols, &object.names, symtab.symbols(), symbol)
                        // alloc: error path
                        .map_err(|why| format!("{}: {why}", place(&object)))?;
                    make(apply, size, None)
                }
                How::GotBase(apply) => {
                    make(apply, added.got(&mut object.symbols, &mut object.names).map_err(full)?, None)
                }
                How::GotOff(apply) => {
                    let got = added.got(&mut object.symbols, &mut object.names).map_err(full)?;
                    make(apply, symbol, Some(got))
                }
            };
            object.fixups.push(fixup);
        }
        object.sections[target].fixups = start..end(name, object.fixups.len(), "relocations")?;
        run = Some((target, start));
    }
    Ok(object)
}

/// The section at ELF index `index`, without its fixups; its name goes in `names`.
fn section(
    arch: Arch,
    name: &str,
    shstrtab: &mut Table<'_>,
    index: usize,
    section: &elf::SectionHeader64<Endianness>,
    data: &[u8],
    names: &mut Names,
) -> Result<Section, String> {
    let sh_type = section.sh_type(ENDIAN);
    let flags = section.sh_flags(ENDIAN);
    let flag = |flag: elf::SectionFlags| flags.contains(flag);
    let interned = shstrtab.name(names, name, "the section name", section.sh_name(ENDIAN))?;
    let section_name = names.at(interned);
    let align = section.sh_addralign(ENDIAN).max(1);
    let dropped = index == 0
        || !flag(elf::SHF_ALLOC)
        || flag(elf::SHF_EXCLUDE)
        || matches!(
            sh_type,
            elf::SHT_GROUP
                | elf::SHT_SYMTAB
                | elf::SHT_SYMTAB_SHNDX
                | elf::SHT_STRTAB
                | elf::SHT_RELA
                | elf::SHT_REL
                | elf::SHT_NOTE
        )
        || section_name.starts_with(".note")
        || section_name == ".comment"
        || section_name == ".llvm_addrsig"
        || section_name.starts_with(".debug_");
    if dropped {
        return Ok(Section {
            name: interned,
            what: SectionKind::Dropped,
            align,
            data: Data::Zero(0),
            fixups: 0..0,
            retain: false,
        });
    }
    if flag(elf::SHF_COMPRESSED) {
        // alloc: error path
        return Err(format!(
            "{name}: section {section_name} is compressed (SHF_COMPRESSED), which a loaded \
             section may not be"
        ));
    }
    match sh_type {
        elf::SHT_PROGBITS | elf::SHT_NOBITS | elf::SHT_INIT_ARRAY | elf::SHT_FINI_ARRAY | elf::SHT_PREINIT_ARRAY => {}
        elf::SHT_X86_64_UNWIND if arch == Arch::X86_64 => {}
        other => {
            // alloc: error path
            return Err(format!(
                "{name}: section {section_name} is loaded (SHF_ALLOC) and of type {:#x}, which \
                 blaze-link does not lay out",
                other.0
            ));
        }
    }
    let what = if section_name == ".eh_frame"
        || (arch == Arch::X86_64 && sh_type == elf::SHT_X86_64_UNWIND)
    {
        SectionKind::EhFrame
    } else {
        let (class, rank) = super::section(
            &section_name,
            flag(elf::SHF_WRITE),
            flag(elf::SHF_EXECINSTR),
            flag(elf::SHF_TLS),
        )
        // alloc: error path
        .map_err(|why| format!("{name}: section {section_name}: {why}"))?;
        SectionKind::Alloc(class, rank)
    };
    let data = if sh_type == elf::SHT_NOBITS {
        Data::Zero(section.sh_size(ENDIAN))
    } else {
        // Bounds-checked against the input here, so the range below can't pass its end.
        let bytes = section
            .data(ENDIAN, data)
            // alloc: error path
            .map_err(|error| format!("{name}: section {section_name}: {error}"))?;
        let start = section.sh_offset(ENDIAN);
        Data::Input(start..start + bytes.len() as u64)
    };
    Ok(Section {
        name: interned,
        what,
        align,
        data,
        fixups: 0..0,
        retain: flag(elf::SHF_GNU_RETAIN),
    })
}

/// What a relocation type becomes.
enum How {
    Nothing,
    To(Apply),
    /// Against the object's module symbol: offset 0 of its TLS block, whose GD pair is the
    /// module's (`R_X86_64_TLSLD`).
    Module(Apply),
    /// Against an absolute symbol of the target's size (`R_X86_64_SIZE*`).
    Size(Apply),
    /// Against `_GLOBAL_OFFSET_TABLE_` (`R_X86_64_GOTPC*`).
    GotBase(Apply),
    /// Against the target, minus `_GLOBAL_OFFSET_TABLE_` (`R_X86_64_GOTOFF64`).
    GotOff(Apply),
}

fn x86_64(r_type: elf::RelocationType) -> Result<How, &'static str> {
    Ok(match r_type {
        elf::R_X86_64_NONE => How::Nothing,
        elf::R_X86_64_64 => How::To(Apply::Abs64),
        elf::R_X86_64_32 => How::To(Apply::Abs32),
        elf::R_X86_64_32S => How::To(Apply::Abs32S),
        elf::R_X86_64_PC32 => How::To(Apply::Rel32),
        elf::R_X86_64_PC64 => How::To(Apply::Rel64),
        elf::R_X86_64_PLT32 => How::To(Apply::X86Branch32),
        elf::R_X86_64_GOTPCREL | elf::R_X86_64_GOTPCRELX | elf::R_X86_64_REX_GOTPCRELX => {
            How::To(Apply::X86GotPcRel32)
        }
        elf::R_X86_64_TPOFF32 => How::To(Apply::X86TpOff32),
        elf::R_X86_64_GOTTPOFF => How::To(Apply::X86GotTpOff32),
        elf::R_X86_64_TLSGD => How::To(Apply::X86TlsGd32),
        elf::R_X86_64_TLSLD => How::Module(Apply::X86TlsGd32),
        elf::R_X86_64_DTPOFF32 => How::To(Apply::DtpOff32),
        elf::R_X86_64_DTPOFF64 => How::To(Apply::DtpOff64),
        elf::R_X86_64_SIZE32 => How::Size(Apply::Abs32),
        elf::R_X86_64_SIZE64 => How::Size(Apply::Abs64),
        elf::R_X86_64_GOTPC32 => How::GotBase(Apply::Rel32),
        elf::R_X86_64_GOTPC64 => How::GotBase(Apply::Rel64),
        elf::R_X86_64_GOTOFF64 => How::GotOff(Apply::Delta64),
        elf::R_X86_64_8 | elf::R_X86_64_16 | elf::R_X86_64_PC8 | elf::R_X86_64_PC16 => {
            How::To(Apply::Native(r_type.0))
        }
        elf::R_X86_64_GOTPC32_TLSDESC | elf::R_X86_64_TLSDESC_CALL => {
            return Err("TLS descriptors (-mtls-dialect=gnu2) are not supported");
        }
        elf::R_X86_64_GOT32
        | elf::R_X86_64_GOT64
        | elf::R_X86_64_GOTPCREL64
        | elf::R_X86_64_GOTPLT64
        | elf::R_X86_64_PLTOFF64 => return Err("the large code model is not supported"),
        _ => return Err("not a relocation type blaze-link reads"),
    })
}

fn aarch64(r_type: elf::RelocationType) -> Result<How, &'static str> {
    Ok(How::To(match r_type {
        elf::R_AARCH64_NONE => return Ok(How::Nothing),
        elf::R_AARCH64_ABS64 => Apply::Abs64,
        elf::R_AARCH64_ABS32 => Apply::Abs32,
        elf::R_AARCH64_PREL64 => Apply::Rel64,
        elf::R_AARCH64_PREL32 => Apply::Rel32,
        elf::R_AARCH64_CALL26 => Apply::A64Call26,
        elf::R_AARCH64_JUMP26 => Apply::A64Jump26,
        elf::R_AARCH64_ADR_PREL_PG_HI21 => Apply::A64Page21,
        elf::R_AARCH64_ADR_PREL_LO21 => Apply::A64Adr21,
        elf::R_AARCH64_ADD_ABS_LO12_NC | elf::R_AARCH64_LDST8_ABS_LO12_NC => {
            Apply::A64Lo12 { shift: 0 }
        }
        elf::R_AARCH64_LDST16_ABS_LO12_NC => Apply::A64Lo12 { shift: 1 },
        elf::R_AARCH64_LDST32_ABS_LO12_NC => Apply::A64Lo12 { shift: 2 },
        elf::R_AARCH64_LDST64_ABS_LO12_NC => Apply::A64Lo12 { shift: 3 },
        elf::R_AARCH64_LDST128_ABS_LO12_NC => Apply::A64Lo12 { shift: 4 },
        elf::R_AARCH64_CONDBR19 => Apply::A64CondBr19,
        elf::R_AARCH64_TSTBR14 => Apply::A64TstBr14,
        elf::R_AARCH64_MOVW_UABS_G0 | elf::R_AARCH64_MOVW_UABS_G0_NC => Apply::A64MovW { group: 0 },
        elf::R_AARCH64_MOVW_UABS_G1 | elf::R_AARCH64_MOVW_UABS_G1_NC => Apply::A64MovW { group: 1 },
        elf::R_AARCH64_MOVW_UABS_G2 | elf::R_AARCH64_MOVW_UABS_G2_NC => Apply::A64MovW { group: 2 },
        elf::R_AARCH64_MOVW_UABS_G3 => Apply::A64MovW { group: 3 },
        elf::R_AARCH64_ADR_GOT_PAGE => Apply::A64GotPage21,
        elf::R_AARCH64_LD64_GOT_LO12_NC => Apply::A64GotLo12,
        elf::R_AARCH64_TLSLE_ADD_TPREL_HI12 => Apply::A64TpHi12,
        elf::R_AARCH64_TLSLE_ADD_TPREL_LO12 | elf::R_AARCH64_TLSLE_ADD_TPREL_LO12_NC => {
            Apply::A64TpLo12
        }
        elf::R_AARCH64_TLSIE_ADR_GOTTPREL_PAGE21 => Apply::A64TlsIePage21,
        elf::R_AARCH64_TLSIE_LD64_GOTTPREL_LO12_NC => Apply::A64TlsIeLo12,
        elf::R_AARCH64_TLSDESC_ADR_PAGE21 => Apply::A64TlsDescPage21,
        elf::R_AARCH64_TLSDESC_LD64_LO12 => Apply::A64TlsDescLo12,
        elf::R_AARCH64_TLSDESC_ADD_LO12 => Apply::A64TlsDescAdd12,
        elf::R_AARCH64_TLSDESC_CALL => Apply::A64TlsDescCall,
        elf::R_AARCH64_ABS16
        | elf::R_AARCH64_PREL16
        | elf::R_AARCH64_ADR_PREL_PG_HI21_NC
        | elf::R_AARCH64_LD_PREL_LO19
        | elf::R_AARCH64_MOVW_SABS_G0
        | elf::R_AARCH64_MOVW_SABS_G1
        | elf::R_AARCH64_MOVW_SABS_G2
        | elf::R_AARCH64_MOVW_PREL_G0
        | elf::R_AARCH64_MOVW_PREL_G0_NC
        | elf::R_AARCH64_MOVW_PREL_G1
        | elf::R_AARCH64_MOVW_PREL_G1_NC
        | elf::R_AARCH64_MOVW_PREL_G2
        | elf::R_AARCH64_MOVW_PREL_G2_NC
        | elf::R_AARCH64_MOVW_PREL_G3 => Apply::Native(r_type.0),
        _ => return Err("not a relocation type blaze-link reads"),
    }))
}

/// The local and undefined symbols an object's relocations add after its ELF ones.
struct Added {
    /// The name of the unnamed ones.
    empty: Name,
    module: Option<u32>,
    got: Option<u32>,
    /// By the ELF symbol whose size each holds.
    sizes: HashMap<u32, u32>,
}

impl Added {
    fn new(empty: Name) -> Added {
        Added { empty, module: None, got: None, sizes: HashMap::default() }
    }

    fn push(symbols: &mut Vec<Symbol>, symbol: Symbol) -> u32 {
        symbols.push(symbol);
        (symbols.len() - 1) as u32
    }

    fn module(&mut self, symbols: &mut Vec<Symbol>) -> u32 {
        let empty = self.empty;
        *self.module.get_or_insert_with(|| {
            Self::push(symbols, Symbol {
                name: empty,
                def: SymbolDef::Absolute(0),
                binding: Binding::Local,
                kind: SymbolKind::Tls,
                hidden: true,
            })
        })
    }

    fn got(&mut self, symbols: &mut Vec<Symbol>, names: &mut Names) -> Result<u32, String> {
        if let Some(got) = self.got {
            return Ok(got);
        }
        let got = Self::push(symbols, Symbol {
            name: names.push(GOT)?,
            def: SymbolDef::Undefined,
            binding: Binding::Global,
            kind: SymbolKind::Other,
            hidden: false,
        });
        self.got = Some(got);
        Ok(got)
    }

    /// A symbol whose value is the size of ELF symbol `target` (`elf` is the object's ELF
    /// symbols), which must be this object's and final: a weak or common one may be replaced
    /// by a bigger one elsewhere.
    fn size(
        &mut self,
        symbols: &mut Vec<Symbol>,
        names: &Names,
        elf: &[Sym],
        target: u32,
    ) -> Result<u32, String> {
        let entry = match self.sizes.entry(target) {
            Entry::Occupied(it) => return Ok(*it.get()),
            Entry::Vacant(it) => it,
        };
        let of = &symbols[target as usize];
        match (of.def, of.binding) {
            (SymbolDef::Undefined, _) => {
                // alloc: error path
                return Err(format!(
                    "the size of {}, which the object does not define",
                    names.at(of.name)
                ));
            }
            (SymbolDef::Common { .. }, _) => {
                // alloc: error path
                return Err(format!(
                    "the size of common symbol {}, which is known only once the link merges it",
                    names.at(of.name)
                ));
            }
            (_, Binding::Weak) => {
                // alloc: error path
                return Err(format!(
                    "the size of weak symbol {}, which another object may replace",
                    names.at(of.name)
                ));
            }
            _ => {}
        }
        let symbol = Self::push(symbols, Symbol {
            name: self.empty,
            def: SymbolDef::Absolute(elf[target as usize].st_size(ENDIAN)),
            binding: Binding::Local,
            kind: SymbolKind::Other,
            hidden: true,
        });
        Ok(*entry.insert(symbol))
    }
}

macro_rules! names {
    ($($name:ident),* $(,)?) => {
        &[$((elf::$name, stringify!($name))),*]
    };
}

const X86_64_NAMES: &[(elf::RelocationType, &str)] = names![
    R_X86_64_NONE, R_X86_64_64, R_X86_64_PC32, R_X86_64_GOT32, R_X86_64_PLT32, R_X86_64_COPY,
    R_X86_64_GLOB_DAT, R_X86_64_JUMP_SLOT, R_X86_64_RELATIVE, R_X86_64_GOTPCREL, R_X86_64_32,
    R_X86_64_32S, R_X86_64_16, R_X86_64_PC16, R_X86_64_8, R_X86_64_PC8, R_X86_64_DTPMOD64,
    R_X86_64_DTPOFF64, R_X86_64_TPOFF64, R_X86_64_TLSGD, R_X86_64_TLSLD, R_X86_64_DTPOFF32,
    R_X86_64_GOTTPOFF, R_X86_64_TPOFF32, R_X86_64_PC64, R_X86_64_GOTOFF64, R_X86_64_GOTPC32,
    R_X86_64_GOT64, R_X86_64_GOTPCREL64, R_X86_64_GOTPC64, R_X86_64_GOTPLT64, R_X86_64_PLTOFF64,
    R_X86_64_SIZE32, R_X86_64_SIZE64, R_X86_64_GOTPC32_TLSDESC, R_X86_64_TLSDESC_CALL,
    R_X86_64_TLSDESC, R_X86_64_IRELATIVE, R_X86_64_RELATIVE64, R_X86_64_GOTPCRELX,
    R_X86_64_REX_GOTPCRELX,
];

const AARCH64_NAMES: &[(elf::RelocationType, &str)] = names![
    R_AARCH64_NONE, R_AARCH64_ABS64, R_AARCH64_ABS32, R_AARCH64_ABS16, R_AARCH64_PREL64,
    R_AARCH64_PREL32, R_AARCH64_PREL16, R_AARCH64_MOVW_UABS_G0, R_AARCH64_MOVW_UABS_G0_NC,
    R_AARCH64_MOVW_UABS_G1, R_AARCH64_MOVW_UABS_G1_NC, R_AARCH64_MOVW_UABS_G2,
    R_AARCH64_MOVW_UABS_G2_NC, R_AARCH64_MOVW_UABS_G3, R_AARCH64_MOVW_SABS_G0,
    R_AARCH64_MOVW_SABS_G1, R_AARCH64_MOVW_SABS_G2, R_AARCH64_LD_PREL_LO19,
    R_AARCH64_ADR_PREL_LO21, R_AARCH64_ADR_PREL_PG_HI21, R_AARCH64_ADR_PREL_PG_HI21_NC,
    R_AARCH64_ADD_ABS_LO12_NC, R_AARCH64_LDST8_ABS_LO12_NC, R_AARCH64_TSTBR14,
    R_AARCH64_CONDBR19, R_AARCH64_JUMP26, R_AARCH64_CALL26, R_AARCH64_LDST16_ABS_LO12_NC,
    R_AARCH64_LDST32_ABS_LO12_NC, R_AARCH64_LDST64_ABS_LO12_NC, R_AARCH64_MOVW_PREL_G0,
    R_AARCH64_MOVW_PREL_G0_NC, R_AARCH64_MOVW_PREL_G1, R_AARCH64_MOVW_PREL_G1_NC,
    R_AARCH64_MOVW_PREL_G2, R_AARCH64_MOVW_PREL_G2_NC, R_AARCH64_MOVW_PREL_G3,
    R_AARCH64_LDST128_ABS_LO12_NC, R_AARCH64_MOVW_GOTOFF_G0, R_AARCH64_MOVW_GOTOFF_G0_NC,
    R_AARCH64_MOVW_GOTOFF_G1, R_AARCH64_MOVW_GOTOFF_G1_NC, R_AARCH64_MOVW_GOTOFF_G2,
    R_AARCH64_MOVW_GOTOFF_G2_NC, R_AARCH64_MOVW_GOTOFF_G3, R_AARCH64_GOTREL64,
    R_AARCH64_GOTREL32, R_AARCH64_GOT_LD_PREL19, R_AARCH64_LD64_GOTOFF_LO15,
    R_AARCH64_ADR_GOT_PAGE, R_AARCH64_LD64_GOT_LO12_NC, R_AARCH64_LD64_GOTPAGE_LO15,
    R_AARCH64_TLSGD_ADR_PREL21, R_AARCH64_TLSGD_ADR_PAGE21, R_AARCH64_TLSGD_ADD_LO12_NC,
    R_AARCH64_TLSGD_MOVW_G1, R_AARCH64_TLSGD_MOVW_G0_NC, R_AARCH64_TLSLD_ADR_PREL21,
    R_AARCH64_TLSLD_ADR_PAGE21, R_AARCH64_TLSLD_ADD_LO12_NC, R_AARCH64_TLSLD_MOVW_G1,
    R_AARCH64_TLSLD_MOVW_G0_NC, R_AARCH64_TLSLD_LD_PREL19, R_AARCH64_TLSLD_MOVW_DTPREL_G2,
    R_AARCH64_TLSLD_MOVW_DTPREL_G1, R_AARCH64_TLSLD_MOVW_DTPREL_G1_NC,
    R_AARCH64_TLSLD_MOVW_DTPREL_G0, R_AARCH64_TLSLD_MOVW_DTPREL_G0_NC,
    R_AARCH64_TLSLD_ADD_DTPREL_HI12, R_AARCH64_TLSLD_ADD_DTPREL_LO12,
    R_AARCH64_TLSLD_ADD_DTPREL_LO12_NC, R_AARCH64_TLSLD_LDST8_DTPREL_LO12,
    R_AARCH64_TLSLD_LDST8_DTPREL_LO12_NC, R_AARCH64_TLSLD_LDST16_DTPREL_LO12,
    R_AARCH64_TLSLD_LDST16_DTPREL_LO12_NC, R_AARCH64_TLSLD_LDST32_DTPREL_LO12,
    R_AARCH64_TLSLD_LDST32_DTPREL_LO12_NC, R_AARCH64_TLSLD_LDST64_DTPREL_LO12,
    R_AARCH64_TLSLD_LDST64_DTPREL_LO12_NC, R_AARCH64_TLSIE_MOVW_GOTTPREL_G1,
    R_AARCH64_TLSIE_MOVW_GOTTPREL_G0_NC, R_AARCH64_TLSIE_ADR_GOTTPREL_PAGE21,
    R_AARCH64_TLSIE_LD64_GOTTPREL_LO12_NC, R_AARCH64_TLSIE_LD_GOTTPREL_PREL19,
    R_AARCH64_TLSLE_MOVW_TPREL_G2, R_AARCH64_TLSLE_MOVW_TPREL_G1,
    R_AARCH64_TLSLE_MOVW_TPREL_G1_NC, R_AARCH64_TLSLE_MOVW_TPREL_G0,
    R_AARCH64_TLSLE_MOVW_TPREL_G0_NC, R_AARCH64_TLSLE_ADD_TPREL_HI12,
    R_AARCH64_TLSLE_ADD_TPREL_LO12, R_AARCH64_TLSLE_ADD_TPREL_LO12_NC,
    R_AARCH64_TLSLE_LDST8_TPREL_LO12, R_AARCH64_TLSLE_LDST8_TPREL_LO12_NC,
    R_AARCH64_TLSLE_LDST16_TPREL_LO12, R_AARCH64_TLSLE_LDST16_TPREL_LO12_NC,
    R_AARCH64_TLSLE_LDST32_TPREL_LO12, R_AARCH64_TLSLE_LDST32_TPREL_LO12_NC,
    R_AARCH64_TLSLE_LDST64_TPREL_LO12, R_AARCH64_TLSLE_LDST64_TPREL_LO12_NC,
    R_AARCH64_TLSDESC_LD_PREL19, R_AARCH64_TLSDESC_ADR_PREL21, R_AARCH64_TLSDESC_ADR_PAGE21,
    R_AARCH64_TLSDESC_LD64_LO12, R_AARCH64_TLSDESC_ADD_LO12, R_AARCH64_TLSDESC_OFF_G1,
    R_AARCH64_TLSDESC_OFF_G0_NC, R_AARCH64_TLSDESC_LDR, R_AARCH64_TLSDESC_ADD,
    R_AARCH64_TLSDESC_CALL, R_AARCH64_TLSLE_LDST128_TPREL_LO12,
    R_AARCH64_TLSLE_LDST128_TPREL_LO12_NC, R_AARCH64_TLSLD_LDST128_DTPREL_LO12,
];

/// The name of relocation type `r_type` of `arch`, for errors.
fn relocation_name(arch: Arch, r_type: elf::RelocationType) -> &'static str {
    let names: &[(elf::RelocationType, &'static str)] = match arch {
        Arch::X86_64 => X86_64_NAMES,
        Arch::Aarch64 => AARCH64_NAMES,
        Arch::Wasm32 => &[],
    };
    names.iter().find(|&&(number, _)| number == r_type).map_or("an unknown type", |&(_, name)| name)
}
