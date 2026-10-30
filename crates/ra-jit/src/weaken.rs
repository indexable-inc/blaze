//! `ra-jit weaken FILE...`: copies of rustc's Mach-O objects and archives whose definitions
//! give way to ra-jit's.
//!
//! ra-jit's object is linked ahead of what rustc built of the same crates, and defines many of
//! the same names. An ELF linker is told to keep the first definition it meets. Apple's has no
//! such flag: two definitions of a name are an error unless one is weak, and of several weak
//! ones the first on the command line stays. So every definition of the copies is made weak,
//! and private to the program as well, which keeps the loader from looking for the name in
//! the libraries it loads. rustc's copy of a static that is private to its object (rustc
//! keeps a static to the codegen unit that uses it, and LLVM may rename it) is made private to
//! the program instead, so that ra-jit's code can share it (`symbols::StaticCopy`). Nothing
//! else of a file changes, and a file that is no Mach-O object is copied as it is.

use std::{
    ops::Range,
    path::{Path, PathBuf},
};

use anyhow::Context as _;
use object::{
    LittleEndian as LE,
    macho::{LC_SYMTAB, MachHeader64, N_EXT, N_PEXT, N_SECT, N_STAB, N_TYPE, N_WEAK_DEF, Nlist64},
    pod,
    read::{
        archive::ArchiveFile,
        macho::{MachHeader as _, Nlist as _, Segment as _},
    },
};

/// The directory of the copies, in the one the files are in. Named anew when the copies
/// change, so that none of an older ra-jit is taken for one of this.
pub(crate) const DIR: &str = "ra-jit-weak-2";

/// The directories of copies older ra-jits wrote, which this one deletes.
const OLD_DIRS: [&str; 1] = ["ra-jit-weak"];

/// Writes the copy of each file and prints where it is, in the order given.
pub(crate) fn command(args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    for file in args {
        println!("{}", copy_of(Path::new(&file))?.display());
    }
    Ok(())
}

/// Writes the copy of `file`, unless the one there is of the file as it is, and says where
/// it is.
pub(crate) fn copy_of(file: &Path) -> anyhow::Result<PathBuf> {
    let (Some(dir), Some(name)) = (file.parent(), file.file_name()) else {
        anyhow::bail!("{} names no file", file.display());
    };
    for old in OLD_DIRS {
        _ = std::fs::remove_dir_all(dir.join(old));
    }
    let dir = dir.join(DIR);
    std::fs::create_dir_all(&dir)?;
    let copy = dir.join(name);
    if !up_to_date(&copy, file) {
        let mut data =
            std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
        weaken(&mut data);
        crate::replace(&copy, &data)?;
    }
    Ok(copy)
}

/// Whether the copy was written after the file it is of.
fn up_to_date(copy: &Path, file: &Path) -> bool {
    let written = |path: &Path| std::fs::metadata(path).and_then(|it| it.modified()).ok();
    matches!((written(copy), written(file)), (Some(copy), Some(file)) if copy > file)
}

/// Makes the definitions of an object, or of each object of an archive, weak.
fn weaken(data: &mut [u8]) {
    let members: Vec<Range<usize>> = match ArchiveFile::parse(&*data) {
        Ok(archive) => archive
            .members()
            .filter_map(|member| {
                let (start, len) = member.ok()?.file_range();
                Some(start as usize..(start + len) as usize)
            })
            .collect(),
        Err(_) => vec![0..data.len()],
    };
    for member in members {
        definitions(&mut data[member]);
    }
}

/// What becomes of a definition.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Made {
    /// It stays as it is.
    Kept,
    /// Weak, and still a name the program can export.
    Weak,
    /// Weak and private to the program.
    Private,
}

/// Makes the definitions of a Mach-O object weak, and its statics private to the program;
/// an object of another format stays as it is.
pub(crate) fn definitions(data: &mut [u8]) {
    weaken_but(data, |_| Made::Private);
    share_statics(data);
}

/// Makes each symbol of Rust's that is private to the object and names data the program
/// writes (a section `symbols::CopyOfStatic` reads as such) a private external, which the
/// code of another object can refer to: rustc's copy of a static it kept to its codegen unit,
/// which ra-jit's code has to share. Its place in the table stays, among the local ones,
/// which Apple's linker takes as it is, so no relocation's symbol changes; nor is it weak,
/// so that two of one name are an error, not one.
fn share_statics(data: &mut [u8]) {
    let written = sections_written(data);
    let Some((symbols, strings)) = symbol_table(data) else { return };
    let (Some(names), Some(_)) = (data.get(strings.clone()), data.get(symbols.clone())) else {
        return;
    };
    let names = names.to_vec();
    let Ok(symbols) = pod::slice_from_all_bytes_mut::<Nlist64<LE>>(&mut data[symbols]) else {
        return;
    };
    for symbol in symbols {
        let defined = symbol.n_type & (N_STAB | N_TYPE) == N_SECT;
        if !defined || symbol.n_type & N_EXT != 0 {
            continue;
        }
        if !written.get(usize::from(symbol.n_sect).wrapping_sub(1)).is_some_and(|it| *it) {
            continue;
        }
        let name = names.get(symbol.n_strx.get(LE) as usize..).unwrap_or_default();
        let name = name.split(|&it| it == 0).next().unwrap_or_default();
        let name = std::str::from_utf8(name).ok().and_then(|it| it.strip_prefix('_'));
        if name.is_some_and(|it| crate::symbols::rust_symbol_length(it).is_some()) {
            symbol.n_type |= N_EXT | N_PEXT;
        }
    }
}

/// rustc's file as the daemon links it on an ELF system: a copy of it in which every symbol
/// of Rust's that is private to its object and names data the program writes is a hidden
/// global instead, or `file` itself when it has none. An ELF linker keeps the first
/// definition of a name, so nothing else of rustc's has to give way to the daemon's; but its
/// copy of a static that rustc kept to the codegen unit that uses it (and LLVM may have
/// renamed) is one the daemon's code has to share (`symbols::StaticCopy`), and a symbol
/// private to an object is one no other object can refer to. Hidden, the name stays the
/// program's own; global, two definitions of one name are the linker's first-stays rule, as
/// for every other name of rustc's, of which a static has one.
pub(crate) fn shared(file: &Path) -> anyhow::Result<PathBuf> {
    let (Some(dir), Some(name)) = (file.parent(), file.file_name()) else {
        anyhow::bail!("{} names no file", file.display());
    };
    let dir = dir.join(DIR);
    let copy = dir.join(name);
    // That the file has nothing to share, written so that the next link need not look again.
    let mut as_is = name.to_owned();
    as_is.push(".as-is");
    let as_is = dir.join(as_is);
    if up_to_date(&copy, file) {
        return Ok(copy);
    }
    if up_to_date(&as_is, file) {
        return Ok(file.to_owned());
    }
    let data = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    std::fs::create_dir_all(&dir)?;
    match share_elf(&data).with_context(|| format!("sharing the statics of {}", file.display()))? {
        Some(shared) => {
            _ = std::fs::remove_file(&as_is);
            crate::replace(&copy, &shared)?;
            Ok(copy)
        }
        None => {
            _ = std::fs::remove_file(&copy);
            crate::replace(&as_is, b"")?;
            Ok(file.to_owned())
        }
    }
}

/// [`shared`]'s copy of an ELF object or of an archive of them, when there is one to make.
/// An archive is written again whole, with its index of names, the members it had in the
/// order they were in.
fn share_elf(data: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
    let Ok(archive) = ArchiveFile::parse(data) else { return share_elf_object(data) };
    let mut members = Vec::new();
    let mut any = false;
    for member in archive.members() {
        let member = member?;
        let name = String::from_utf8_lossy(member.name()).into_owned();
        let bytes = member.data(data)?;
        let shared = share_elf_object(bytes)?;
        any |= shared.is_some();
        members.push((
            name,
            shared.map_or(std::borrow::Cow::Borrowed(bytes), std::borrow::Cow::Owned),
        ));
    }
    if !any {
        return Ok(None);
    }
    let members: Vec<ar_archive_writer::NewArchiveMember<'_>> = members
        .iter()
        .map(|(name, bytes)| {
            ar_archive_writer::NewArchiveMember::new(
                &**bytes,
                &ar_archive_writer::DEFAULT_OBJECT_READER,
                name.clone(),
            )
        })
        .collect();
    let mut out = std::io::Cursor::new(Vec::new());
    ar_archive_writer::write_archive_to_stream(
        &mut out,
        &members,
        ar_archive_writer::ArchiveKind::Gnu,
        false,
        None,
    )?;
    Ok(Some(out.into_inner()))
}

/// [`shared`]'s copy of one ELF object, when it has a static to share; anything else (rustc's
/// metadata, another format) has none. A 64-bit little-endian object is changed in place, as
/// object's ELF builder takes no section groups, which rustc's objects have: the symbols made
/// global move to the end of the local ones, which leaves the global ones where they were, and
/// what numbers a local symbol (relocations, a group's signature, the table of extended section
/// indices) is numbered again. LLVM's tables of which symbols' addresses matter and of the call
/// graph's weights are made ones the linker drops (they are `SHF_EXCLUDE`): without the first,
/// every symbol's address matters, which is the safe side.
fn share_elf_object(data: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
    use object::elf;
    if !data.starts_with(&elf::ELFMAG)
        || data.get(4) != Some(&elf::ELFCLASS64)
        || data.get(5) != Some(&elf::ELFDATA2LSB)
    {
        return Ok(None);
    }
    let mut data = data.to_vec();
    Ok(promote_statics(&mut data)?.then_some(data))
}

/// LLVM's section type for the table of symbols whose addresses matter (`.llvm_addrsig`).
const SHT_LLVM_ADDRSIG: u32 = 0x6fff_4c03;

/// One section header of a 64-bit object, as [`promote_statics`] reads it.
#[derive(Clone, Copy)]
struct Header {
    at: usize,
    kind: u32,
    flags: u64,
    offset: usize,
    size: usize,
    link: u32,
    info: u32,
    name: u32,
}

/// Makes the statics of [`share_elf_object`] hidden globals in `data`, a 64-bit
/// little-endian ELF object, and says whether there were any.
fn promote_statics(data: &mut [u8]) -> anyhow::Result<bool> {
    use object::elf;
    let u16_at = |data: &[u8], at: usize| -> anyhow::Result<u16> {
        Ok(u16::from_le_bytes(data.get(at..at + 2).context("truncated")?.try_into()?))
    };
    let u32_at = |data: &[u8], at: usize| -> anyhow::Result<u32> {
        Ok(u32::from_le_bytes(data.get(at..at + 4).context("truncated")?.try_into()?))
    };
    let u64_at = |data: &[u8], at: usize| -> anyhow::Result<u64> {
        Ok(u64::from_le_bytes(data.get(at..at + 8).context("truncated")?.try_into()?))
    };
    let (shoff, shentsize) = (u64_at(data, 0x28)? as usize, usize::from(u16_at(data, 0x3a)?));
    let (mut shnum, mut shstrndx) =
        (usize::from(u16_at(data, 0x3c)?), usize::from(u16_at(data, 0x3e)?));
    if shoff == 0 || shentsize < 64 {
        return Ok(false);
    }
    // More sections than the header can count put the count in the first one's size, and
    // the index of the names' section in its link.
    if shnum == 0 {
        shnum = u64_at(data, shoff + 0x20)? as usize;
    }
    if shstrndx == usize::from(elf::SHN_XINDEX) {
        shstrndx = u32_at(data, shoff + 0x28)? as usize;
    }
    let mut headers = Vec::with_capacity(shnum);
    for index in 0..shnum {
        let at = shoff + index * shentsize;
        headers.push(Header {
            at,
            name: u32_at(data, at)?,
            kind: u32_at(data, at + 4)?,
            flags: u64_at(data, at + 8)?,
            offset: u64_at(data, at + 0x18)? as usize,
            size: u64_at(data, at + 0x20)? as usize,
            link: u32_at(data, at + 0x28)?,
            info: u32_at(data, at + 0x2c)?,
        });
    }
    let names = headers.get(shstrndx).context("no section of section names")?;
    let names = data.get(names.offset..names.offset + names.size).context("truncated")?.to_vec();
    let name_of = |header: &Header| {
        let name = names.get(header.name as usize..).unwrap_or_default();
        name.split(|&it| it == 0).next().unwrap_or_default().to_vec()
    };
    let Some(table) = headers.iter().position(|it| it.kind == elf::SHT_SYMTAB) else {
        return Ok(false);
    };
    let symtab = headers[table];
    let strings = *headers.get(symtab.link as usize).context("no names of symbols")?;
    let locals = symtab.info as usize;
    const SYM: usize = 24;
    let count = symtab.size / SYM;
    if locals > count {
        anyhow::bail!("{locals} local symbols of {count}");
    }
    // Which local symbols to make global.
    let mut promoted = vec![false; locals];
    for (index, promote) in promoted.iter_mut().enumerate().skip(1) {
        let at = symtab.offset + index * SYM;
        let (info, shndx) = (*data.get(at + 4).context("truncated")?, u16_at(data, at + 6)?);
        let (bind, kind) = (info >> 4, info & 0xf);
        if bind != elf::STB_LOCAL || !matches!(kind, elf::STT_OBJECT | elf::STT_TLS) {
            continue;
        }
        if shndx == elf::SHN_UNDEF || shndx >= elf::SHN_LORESERVE {
            continue;
        }
        let Some(section) = headers.get(usize::from(shndx)) else { continue };
        let written = section.flags & u64::from(elf::SHF_WRITE) != 0
            && !name_of(section).starts_with(b".data.rel.ro");
        let name_at = strings.offset + u32_at(data, at)? as usize;
        let name = data.get(name_at..strings.offset + strings.size).unwrap_or_default();
        let name = name.split(|&it| it == 0).next().unwrap_or_default();
        *promote = written
            && std::str::from_utf8(name)
                .is_ok_and(|it| crate::symbols::rust_symbol_length(it).is_some());
    }
    let moved = promoted.iter().filter(|it| **it).count();
    if moved == 0 {
        return Ok(false);
    }
    // The new place of each local symbol: the others keep their order, then those made global.
    let kept = locals - moved;
    let mut new_index = vec![0u32; locals];
    let (mut next_kept, mut next_moved) = (0, kept);
    for (index, promote) in promoted.iter().enumerate() {
        let place = if *promote { &mut next_moved } else { &mut next_kept };
        new_index[index] = u32::try_from(*place)?;
        *place += 1;
    }
    let renumber = |index: u32| -> u32 { new_index.get(index as usize).copied().unwrap_or(index) };
    let table_bytes = data[symtab.offset..symtab.offset + locals * SYM].to_vec();
    for (index, promote) in promoted.iter().enumerate() {
        let to = symtab.offset + new_index[index] as usize * SYM;
        data[to..to + SYM].copy_from_slice(&table_bytes[index * SYM..(index + 1) * SYM]);
        if *promote {
            data[to + 4] = (elf::STB_GLOBAL << 4) | (data[to + 4] & 0xf);
            data[to + 5] = (data[to + 5] & !3) | elf::STV_HIDDEN;
        }
    }
    data[symtab.at + 0x2c..symtab.at + 0x30].copy_from_slice(&u32::try_from(kept)?.to_le_bytes());
    let table = u32::try_from(table)?;
    for header in &headers {
        if header.link != table {
            continue;
        }
        match header.kind {
            elf::SHT_RELA | elf::SHT_REL => {
                let size = if header.kind == elf::SHT_RELA { 24 } else { 16 };
                for entry in 0..header.size / size {
                    let at = header.offset + entry * size + 8;
                    let r_info = u64_at(data, at)?;
                    let symbol = renumber((r_info >> 32) as u32);
                    let r_info = (u64::from(symbol) << 32) | (r_info & 0xffff_ffff);
                    data[at..at + 8].copy_from_slice(&r_info.to_le_bytes());
                }
            }
            elf::SHT_GROUP => {
                let symbol = renumber(header.info);
                data[header.at + 0x2c..header.at + 0x30].copy_from_slice(&symbol.to_le_bytes());
            }
            elf::SHT_SYMTAB_SHNDX => {
                let shndx = data[header.offset..header.offset + locals * 4].to_vec();
                for (index, _) in promoted.iter().enumerate() {
                    let to = header.offset + new_index[index] as usize * 4;
                    data[to..to + 4].copy_from_slice(&shndx[index * 4..index * 4 + 4]);
                }
            }
            SHT_LLVM_ADDRSIG => {
                if header.flags & u64::from(elf::SHF_EXCLUDE) == 0 {
                    anyhow::bail!("LLVM's table of significant addresses is linked");
                }
                data[header.at + 4..header.at + 8]
                    .copy_from_slice(&elf::SHT_PROGBITS.to_le_bytes());
            }
            _ => {}
        }
    }
    Ok(true)
}

/// Whether each section of a 64-bit Mach-O object, in the order symbols number them from 1,
/// holds data a program writes to, as `symbols::written_at_run_time` tells it.
fn sections_written(data: &[u8]) -> Vec<bool> {
    let mut written = Vec::new();
    let Ok(header) = MachHeader64::<LE>::parse(data, 0) else { return written };
    let Ok(mut commands) = header.load_commands(LE, data, 0) else { return written };
    while let Ok(Some(command)) = commands.next() {
        let Ok(Some((segment, sections))) = command.segment_64() else { continue };
        let Ok(sections) = segment.sections(LE, sections) else { return Vec::new() };
        for section in sections {
            let name = section.sectname.split(|&it| it == 0).next().unwrap_or_default();
            written.push(matches!(name, b"__data" | b"__bss" | b"__common" | b"__thread_vars"));
        }
    }
    written
}

/// The functions of ra-jit's a `#[global_allocator]` of the crate's is called through.
const ALLOCATOR_FUNCTIONS: &[&str] = &[
    "__ra_jit_global_alloc",
    "__ra_jit_global_dealloc",
    "__ra_jit_global_realloc",
    "__ra_jit_global_alloc_zeroed",
];

/// As [`definitions`], for an object of ra-jit's. Of a program's whole, with a C `main` of
/// ra-jit's, that `main` and the functions a stand-in's allocator calls stay as they are: the
/// stand-ins rustc built have weak ones for cargo's link of them, and of two weak ones Apple's
/// linker may keep the stand-in's, which would start the program at whatever is laid out
/// after its `main`. A patch has no `main`, and gives way to the object it patches.
///
/// A name of C's (`#[no_mangle]`, `#[export_name]`) is weak and not private: a program can
/// export it (bun's `-exported_symbols_list` names its `napi_*` for the addons it loads), and
/// Apple's linker exports no private one. The names of Rust's and of ra-jit's are private.
pub(crate) fn ours(data: &mut [u8]) {
    let has_main = defined(data).iter().any(|it| it == b"_main");
    weaken_but(data, |name| {
        let name = name.strip_prefix(b"_").unwrap_or(name);
        let stays = name == b"main"
            || ALLOCATOR_FUNCTIONS.iter().any(|it| it.as_bytes() == name);
        match () {
            _ if has_main && stays => Made::Kept,
            _ if of_rust(name) => Made::Private,
            _ => Made::Weak,
        }
    });
}

/// Whether a name (without Mach-O's `_`) is a symbol rustc mangled or one of ra-jit's own.
fn of_rust(name: &[u8]) -> bool {
    let v0 = name
        .strip_prefix(b"_R")
        .and_then(|it| it.first())
        .is_some_and(|it| it.is_ascii_alphanumeric());
    name.starts_with(b"_ZN") || v0 || name.starts_with(b"ra-jit.")
}

/// The names an object defines and other objects can see.
fn defined(data: &[u8]) -> Vec<Vec<u8>> {
    let mut data = data.to_vec();
    let mut names = Vec::new();
    weaken_but(&mut data, |name| {
        names.push(name.to_vec());
        Made::Kept
    });
    names
}

fn weaken_but(data: &mut [u8], mut made: impl FnMut(&[u8]) -> Made) {
    let Some((symbols, strings)) = symbol_table(data) else { return };
    let (Some(names), Some(_)) = (data.get(strings.clone()), data.get(symbols.clone())) else {
        return;
    };
    let names = names.to_vec();
    let Ok(symbols) = pod::slice_from_all_bytes_mut::<Nlist64<LE>>(&mut data[symbols]) else {
        return;
    };
    for symbol in symbols {
        let defined = symbol.n_type & (N_STAB | N_TYPE) == N_SECT;
        if !defined || symbol.n_type & N_EXT == 0 {
            continue;
        }
        let name = names.get(symbol.n_strx.get(LE) as usize..).unwrap_or_default();
        let name = name.split(|&it| it == 0).next().unwrap_or_default();
        let made = made(name);
        if made == Made::Private {
            symbol.n_type |= N_PEXT;
        }
        if made != Made::Kept {
            symbol.n_desc.set(LE, symbol.n_desc(LE) | N_WEAK_DEF);
        }
    }
}

/// Where in a 64-bit Mach-O object its symbols are, and their names.
fn symbol_table(data: &[u8]) -> Option<(Range<usize>, Range<usize>)> {
    let header = MachHeader64::<LE>::parse(data, 0).ok()?;
    let mut commands = header.load_commands(LE, data, 0).ok()?;
    while let Ok(Some(command)) = commands.next() {
        if command.cmd() != LC_SYMTAB {
            continue;
        }
        let symtab = command.symtab().ok()??;
        let start = symtab.symoff.get(LE) as usize;
        let len = symtab.nsyms.get(LE) as usize * size_of::<Nlist64<LE>>();
        let strings = symtab.stroff.get(LE) as usize;
        let strings = strings..strings + symtab.strsize.get(LE) as usize;
        return Some((start..start + len, strings));
    }
    None
}

#[cfg(test)]
mod tests {
    use object::write::{Mangling, Object, Symbol, SymbolSection};
    use object::{
        Architecture, BinaryFormat, Endianness, SectionKind, SymbolFlags, SymbolKind, SymbolScope,
    };
    use object::{Object as _, ObjectSymbol as _};

    use super::*;

    const NAMES: [&str; 6] = [
        "_main",
        "_napi_create_async_work",
        "___ra_jit_global_alloc",
        "__ZN3app4work17h0123456789abcdefE",
        "__RNvCs1234_3app4work",
        "_ra-jit.0123456789abcdef.app::work",
    ];

    fn object() -> Vec<u8> {
        let mut object =
            Object::new(BinaryFormat::MachO, Architecture::Aarch64, Endianness::Little);
        object.set_mangling(Mangling::None);
        let text = object.add_section(Vec::new(), b"__text".to_vec(), SectionKind::Text);
        for (at, name) in NAMES.iter().enumerate() {
            let offset = object.append_section_data(text, &[0; 4], 4);
            assert_eq!(offset, 4 * at as u64);
            object.add_symbol(Symbol {
                name: name.as_bytes().to_vec(),
                value: offset,
                size: 4,
                kind: SymbolKind::Text,
                scope: SymbolScope::Dynamic,
                weak: false,
                section: SymbolSection::Section(text),
                flags: SymbolFlags::None,
            });
        }
        object.write().unwrap()
    }

    /// Whether each of [`NAMES`] is private and whether it is weak.
    fn made(data: &[u8]) -> Vec<(bool, bool)> {
        let (symbols, strings) = symbol_table(data).unwrap();
        let symbols = pod::slice_from_all_bytes::<Nlist64<LE>>(&data[symbols]).unwrap();
        let named = |name: &str| {
            let symbol = symbols.iter().find(|it| {
                let at = strings.start + it.n_strx.get(LE) as usize;
                data[at..].split(|&it| it == 0).next() == Some(name.as_bytes())
            });
            let symbol = symbol.unwrap();
            (symbol.n_type & N_PEXT != 0, symbol.n_desc(LE) & N_WEAK_DEF != 0)
        };
        NAMES.iter().map(|it| named(it)).collect()
    }

    #[test]
    fn a_name_of_cs_in_ours_can_still_be_exported() {
        let mut data = object();
        ours(&mut data);
        let (private, weak) = (true, true);
        let (kept, exported, mangled) = ((!private, !weak), (!private, weak), (private, weak));
        assert_eq!(made(&data), [kept, exported, kept, mangled, mangled, mangled]);
        let mut data = object();
        definitions(&mut data);
        assert!(made(&data).iter().all(|&it| it == (private, weak)));
    }

    /// rustc's copies of statics it kept to their object, which LLVM may have renamed, become
    /// linkable; the memory a static points to, a thread-local's first value, and what is
    /// not written at run time stay as they are.
    #[test]
    fn a_static_private_to_rustcs_object_is_made_linkable() {
        let mut object =
            Object::new(BinaryFormat::MachO, Architecture::Aarch64, Endianness::Little);
        object.set_mangling(Mangling::None);
        let data = object.add_section(b"__DATA".to_vec(), b"__data".to_vec(), SectionKind::Data);
        let constant =
            object.add_section(b"__TEXT".to_vec(), b"__const".to_vec(), SectionKind::ReadOnlyData);
        let symbols = [
            ("__RNvCs1234_3app7SERVING.0", data, true),
            ("__RNvCs1234_3app6ALLOCS", data, true),
            ("__RNvCs1234_3app6ALLOCS.0.llvm.123", data, true),
            ("__ZN3app6ALLOCS17h0123456789abcdefE.0", data, true),
            ("__RNnCs1234_3app6ALLOCS", data, false),
            ("__RNvCs1234_3app5TABLE", constant, false),
            ("_counter", data, false),
        ];
        for (name, section, _) in symbols {
            let value = object.append_section_data(section, &[0; 8], 8);
            object.add_symbol(Symbol {
                name: name.as_bytes().to_vec(),
                value,
                size: 8,
                kind: SymbolKind::Data,
                scope: SymbolScope::Compilation,
                weak: false,
                section: SymbolSection::Section(section),
                flags: SymbolFlags::None,
            });
        }
        let mut data = object.write().unwrap();
        definitions(&mut data);
        let read = object::File::parse(&*data).unwrap();
        for (name, _, linkable) in symbols {
            let symbol = read.symbols().find(|it| it.name() == Ok(name)).unwrap();
            assert_eq!(symbol.is_global(), linkable, "{name}");
        }
    }

    /// On ELF, rustc's copy of a static private to its object is a hidden global in the copy
    /// of an rlib, which keeps its members in their order, and a relocation to it still names
    /// it; the memory a static points to, and what is not Rust's, stay as they are.
    #[test]
    fn a_static_private_to_rustcs_elf_object_is_made_a_hidden_global() {
        use object::{
            ObjectComdat as _, ObjectSection as _, RelocationTarget, SymbolFlags as Flags, elf,
        };
        let mut object = Object::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
        let data = object.section_id(object::write::StandardSection::Data);
        let constant = object.section_id(object::write::StandardSection::ReadOnlyData);
        let text = object.section_id(object::write::StandardSection::Text);
        let symbols = [
            ("_RNvCs1234_3app7SERVING.0", data, true),
            ("_ZN3app6ALLOCS17h0123456789abcdefE.llvm.123", data, true),
            ("_RNvCs1234_3app5TABLE", constant, false),
            ("counter", data, false),
        ];
        let mut ids = Vec::new();
        for (name, section, _) in symbols {
            let value = object.append_section_data(section, &[0; 8], 8);
            ids.push(object.add_symbol(Symbol {
                name: name.as_bytes().to_vec(),
                value,
                size: 8,
                kind: SymbolKind::Data,
                scope: SymbolScope::Compilation,
                weak: false,
                section: SymbolSection::Section(section),
                flags: SymbolFlags::None,
            }));
        }
        let at = object.append_section_data(text, &[0x90; 16], 16);
        object.add_symbol(Symbol {
            name: b"_RNvCs1234_3app7serving".to_vec(),
            value: at,
            size: 16,
            kind: SymbolKind::Text,
            scope: SymbolScope::Linkage,
            weak: false,
            section: SymbolSection::Section(text),
            flags: SymbolFlags::None,
        });
        object
            .add_relocation(
                text,
                object::write::Relocation {
                    offset: at + 4,
                    symbol: ids[0],
                    addend: -4,
                    flags: object::RelocationFlags::Elf { r_type: elf::R_X86_64_PC32 },
                },
            )
            .unwrap();
        object
            .add_relocation(
                text,
                object::write::Relocation {
                    offset: at + 8,
                    symbol: ids[2],
                    addend: -4,
                    flags: object::RelocationFlags::Elf { r_type: elf::R_X86_64_PC32 },
                },
            )
            .unwrap();
        // rustc's objects have section groups (COMDAT), whose signature may be a local symbol
        // that moves when the statics before it are made global.
        let grouped =
            object.add_section(Vec::new(), b".text.grouped".to_vec(), object::SectionKind::Text);
        let at = object.append_section_data(grouped, &[0x90; 4], 4);
        let signature = object.add_symbol(Symbol {
            name: b"_RNvCs1234_3app7grouped".to_vec(),
            value: at,
            size: 4,
            kind: SymbolKind::Text,
            scope: SymbolScope::Compilation,
            weak: false,
            section: SymbolSection::Section(grouped),
            flags: SymbolFlags::None,
        });
        object.add_comdat(object::write::Comdat {
            kind: object::ComdatKind::Any,
            symbol: signature,
            sections: vec![grouped],
        });
        let object = object.write().unwrap();
        let members = [("lib.rmeta", &b"rust metadata"[..]), ("app.app.1-cgu.0.rcgu.o", &object)];
        let members: Vec<_> = members
            .iter()
            .map(|(name, bytes)| {
                ar_archive_writer::NewArchiveMember::new(
                    *bytes,
                    &ar_archive_writer::DEFAULT_OBJECT_READER,
                    (*name).to_owned(),
                )
            })
            .collect();
        let mut rlib = std::io::Cursor::new(Vec::new());
        ar_archive_writer::write_archive_to_stream(
            &mut rlib,
            &members,
            ar_archive_writer::ArchiveKind::Gnu,
            false,
            None,
        )
        .unwrap();
        let shared = share_elf(&rlib.into_inner()).unwrap().unwrap();
        let archive = ArchiveFile::parse(&*shared).unwrap();
        let members: Vec<_> = archive.members().map(Result::unwrap).collect();
        let names: Vec<_> = members.iter().map(|it| it.name()).collect();
        assert_eq!(names, [&b"lib.rmeta"[..], b"app.app.1-cgu.0.rcgu.o"]);
        assert_eq!(members[0].data(&*shared).unwrap(), b"rust metadata");
        let read = object::File::parse(members[1].data(&*shared).unwrap()).unwrap();
        for (name, _, shared) in symbols {
            let symbol = read.symbols().find(|it| it.name() == Ok(name)).unwrap();
            assert_eq!(symbol.is_global(), shared, "{name}");
            let Flags::Elf { st_other, .. } = symbol.flags() else { panic!("{name}") };
            assert_eq!(st_other & 3 == elf::STV_HIDDEN, shared, "{name}");
        }
        let text = read.section_by_name(".text").unwrap();
        let targets: Vec<_> = text
            .relocations()
            .map(|(_, relocation)| {
                let RelocationTarget::Symbol(target) = relocation.target() else { panic!() };
                read.symbol_by_index(target).unwrap().name().unwrap()
            })
            .collect();
        assert_eq!(targets, [symbols[0].0, symbols[2].0]);
        let group = read.comdats().next().unwrap();
        assert_eq!(
            read.symbol_by_index(group.symbol()).unwrap().name(),
            Ok("_RNvCs1234_3app7grouped")
        );
        // A file with nothing left to share is linked as it is.
        assert!(share_elf(members[1].data(&*shared).unwrap()).unwrap().is_none());
    }
}
