//! The native inputs of a PE link: COFF objects (`.obj`, and `/bigobj` ones), `.lib` archives
//! (the first and second linker members), and import libraries, whose short import members
//! (`IMPORT_OBJECT_HEADER`) and long ones (an object with `.idata$` sections) are read as one
//! `Shared` per DLL with no `__imp_` names: `__imp_X` is `Special::Got { from: 6 }`.
//!
//! Cost: one pass over an object's section headers (which sizes every column) and one over its
//! symbol table, with section names and `.drectve` words borrowed from the input; a symbol's
//! object index is a `u32` column by COFF index. Every input is read in place (`object`'s COFF
//! types are 1-aligned), never copied. A section's bytes are a range of the input
//! (`Data::Input`); every name is borrowed from the input and pushed once into the file's one
//! `Names` arena, sized up front (a section symbol and a COMDAT group reuse the name of their
//! section and leader); relocations and COMDAT members are runs of one column per object. So an
//! object's read allocates a constant number of buffers whatever its size. What is still made
//! is each import's `"<hint> <name>"` (its `Shared::versions` entry), one per import member.
//! An import library read whole parses its members and groups the imports by DLL on every
//! thread; walking the member headers stays one pass, in member order.

use parking_lot::Mutex;
use std::borrow::Cow;
use std::fmt::{self, Write as _};

use blaze_bytes::le::{get_u16 as u16_at, get_u32 as u32_at, get_u64 as u64_at};
use blaze_core::decimal::decimal;
use blaze_graph::par;
use blaze_hash::HashMap;
use blaze_ids::radix;
use blaze_mem::scratch::{self, Lent};
use object::LittleEndian as LE;
use object::pe;
use object::read::SymbolIndex;
use object::read::archive::{ArchiveFile, ArchiveKind};
use object::read::coff::{CoffHeader, SectionTable, Symbol as _, SymbolTable};

use super::{OBJ_PDATA, Pe};
use crate::NativeKind;
use crate::model::{
    Apply, Archive, Binding, Class, Data, Directive, Group, Member, Name, Names, NativeFile,
    NativeFixup, Object, Section, SectionKind, Shared, SharedSymbol, Symbol, SymbolDef, SymbolKind,
    Tie, Writer,
};

/// The size of an archive member's header.
const MEMBER_HEADER: u64 = 60;

const BIGOBJ: [u8; 16] = [
    0xC7, 0xA1, 0xBA, 0xD1, 0xEE, 0xBA, 0xA9, 0x4B, 0xAF, 0x20, 0xFA, 0xF6, 0x6A, 0xA4, 0xDC, 0xB8,
];

// Archive members are 2-aligned and objects come from anywhere: every COFF type read in place
// must be 1-aligned, as `object`'s byte-array fields make them.
const _: () = assert!(
    align_of::<pe::ImageFileHeader>() == 1
        && align_of::<pe::AnonObjectHeaderBigobj>() == 1
        && align_of::<pe::ImageSectionHeader>() == 1
        && align_of::<pe::ImageSymbol>() == 1
        && align_of::<pe::ImageSymbolEx>() == 1
        && align_of::<pe::ImageAuxSymbolSection>() == 1
        && align_of::<pe::ImageAuxSymbolWeak>() == 1
        && align_of::<pe::ImageRelocation>() == 1
);

/// No object symbol: a COFF index that is an aux record or a symbol no relocation can name.
const NONE: u32 = u32::MAX;
/// In [`Reader::groups`], a section that is an associative COMDAT, joined to its root's group
/// once every group is known.
const ASSOCIATIVE: u32 = u32::MAX - 1;

/// The DLL of each `__IMPORT_DESCRIPTOR_<lib>`, by that name.
pub(crate) type Descriptors = HashMap<String, String>;

/// What errors call an input: a file, or a member of an archive (`lib(member)`). Formatted only
/// when an error is.
#[derive(Clone, Copy)]
pub(super) struct Input<'a> {
    file: &'a str,
    member: Option<&'a str>,
}

impl Input<'_> {
    /// The length of what it displays as.
    fn len(self) -> usize {
        self.file.len() + self.member.map_or(0, |it| it.len() + 2)
    }
}

impl fmt::Display for Input<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.member {
            Some(member) => write!(f, "{}({member})", self.file),
            None => f.write_str(self.file),
        }
    }
}

/// An error about `input`, formatted only when it is returned.
fn fail(input: Input<'_>, what: impl fmt::Display) -> String {
    // alloc: error path
    format!("{input}: {what}")
}

/// Bytes of the input as text, borrowed. Bytes that aren't UTF-8 are an error: replaced, they
/// would name another symbol, DLL, member or option.
fn text(bytes: &[u8]) -> Result<Cow<'_, str>, String> {
    std::str::from_utf8(bytes).map(Cow::Borrowed).map_err(|_| {
        // alloc: error path
        format!("the name {:?} is not UTF-8", String::from_utf8_lossy(bytes))
    })
}

/// Where a long import member finds the DLL its `__IMPORT_DESCRIPTOR_` names: the descriptors
/// of every archive read so far (a member the core reads on its own), or those of the import
/// library being read.
#[derive(Clone, Copy)]
enum Dlls<'a> {
    Read(&'a Mutex<Descriptors>),
    Library(&'a Descriptors),
}

impl<'a> Dlls<'a> {
    /// The DLL: borrowed from an import library's own descriptors, copied (once per file: its
    /// `Shared` owns the name) out of the ones behind the lock.
    fn get(self, descriptor: &str) -> Option<Cow<'a, str>> {
        match self {
            Dlls::Read(map) => {
                map.lock().get(descriptor).cloned().map(Cow::Owned)
            }
            Dlls::Library(map) => map.get(descriptor).map(|it| Cow::Borrowed(it.as_str())),
        }
    }
}

/// One import of a DLL as an import member names it: the symbol, borrowed from the member
/// when it can be, and how the DLL's export table names it (`"<hint> <name>"` or
/// `"#<ordinal>"`, which PE carries as the symbol's version).
struct Imported<'a> {
    name: Cow<'a, str>,
    version: String,
    kind: SymbolKind,
}

/// A relocatable input as read: an object, or one import of a DLL.
enum Parsed<'a> {
    Object(Object),
    Import(Cow<'a, str>, Imported<'a>),
}

fn shared(dll: String) -> Shared {
    Shared {
        name: dll,
        symbols: Vec::default(),
        names: Names::default(),
        versions: Vec::default(),
        more: Vec::default(),
        inputs: Vec::default(),
    }
}

/// Adds an import to its DLL: the name pushed into the DLL's names, the export name as a
/// version of its own (each import's differs).
fn add(shared: &mut Shared, input: Input<'_>, import: Imported<'_>) -> Result<(), String> {
    let version = u16::try_from(shared.versions.len()).map_err(|_| {
        fail(input, format_args!("more than 65,536 imports of {}, more than a DLL can export", shared.name))
    })?;
    let name = shared.names.push(&import.name).map_err(|e| fail(input, e))?;
    let export = shared.names.push(&import.version).map_err(|e| fail(input, e))?;
    shared.versions.push(export);
    shared.symbols.push(SharedSymbol { name, version: Some(version), kind: import.kind, weak: false });
    Ok(())
}

pub(super) fn read(pe: &Pe, name: &str, bytes: &[u8], kind: &NativeKind) -> Result<NativeFile, String> {
    let input = Input { file: name, member: None };
    match kind {
        NativeKind::Object => match member(pe, Dlls::Read(&pe.descriptors), input, bytes)? {
            Parsed::Object(object) => Ok(NativeFile::Object(object)),
            Parsed::Import(dll, import) => {
                let mut shared = shared(dll.into_owned());
                add(&mut shared, input, import)?;
                Ok(NativeFile::Shared(shared))
            }
        },
        NativeKind::Archive { whole } => {
            let (archive, descriptors) = archive(input, bytes, *whole)?;
            if !descriptors.is_empty() {
                pe.descriptors.lock().extend(descriptors);
            }
            Ok(NativeFile::Archive(archive))
        }
        NativeKind::Shared { .. } => import_library(pe, input, bytes).map(NativeFile::Shared),
    }
}

/// A relocatable input: an object, or an import member of an archive, read where it is.
fn member<'a>(pe: &Pe, dlls: Dlls<'a>, name: Input<'_>, bytes: &'a [u8]) -> Result<Parsed<'a>, String> {
    if u16_at(bytes, 0) == Some(pe::IMAGE_FILE_MACHINE_UNKNOWN.0) && u16_at(bytes, 2) == Some(0xffff) {
        let version = u16_at(bytes, 4).ok_or_else(|| fail(name, "a truncated COFF header"))?;
        if version == 0 {
            let (dll, symbol) = short_import(name, bytes)?;
            return Ok(Parsed::Import(dll, symbol));
        }
        if version >= 2 && bytes.get(12..28) == Some(&BIGOBJ[..]) {
            return coff::<pe::AnonObjectHeaderBigobj>(pe, dlls, name, bytes);
        }
        return Err(fail(
            name,
            "an anonymous COFF object (one built with /GL for link-time code generation, or /clr): \
             blaze links no LTCG or CLR objects",
        ));
    }
    coff::<pe::ImageFileHeader>(pe, dlls, name, bytes)
}

fn strip(name: &str) -> &str {
    name.strip_prefix(['?', '@', '_']).unwrap_or(name)
}

/// A short import member: one symbol of one DLL, the symbol's and the DLL's names borrowed from it.
fn short_import<'d>(name: Input<'_>, data: &'d [u8]) -> Result<(Cow<'d, str>, Imported<'d>), String> {
    let short = || fail(name, "a truncated import member");
    let machine = u16_at(data, 6).ok_or_else(short)?;
    if machine != pe::IMAGE_FILE_MACHINE_AMD64.0 {
        return Err(fail(name, format_args!("an import member for machine {machine:#x}, not x86-64")));
    }
    let size = u32_at(data, 12).ok_or_else(short)? as usize;
    let ordinal_or_hint = u16_at(data, 16).ok_or_else(short)?;
    let types = pe::ImportObjectFlags(u16_at(data, 18).ok_or_else(short)?);
    let strings = data.get(20..20 + size).ok_or_else(short)?;
    let mut strings = strings.split(|it| *it == 0);
    let symbol = text(strings.next().ok_or_else(short)?).map_err(|e| fail(name, e))?;
    let dll = text(strings.next().ok_or_else(short)?).map_err(|e| fail(name, e))?;
    let kind = match types.import_type() {
        pe::IMPORT_OBJECT_CODE => SymbolKind::Function,
        pe::IMPORT_OBJECT_DATA | pe::IMPORT_OBJECT_CONST => SymbolKind::Data,
        other => return Err(fail(name, format_args!("import type {other} of {symbol}, which no DLL has"))),
    };
    let export_as: Cow<'_, str>;
    let named = |name| ImportName::Name { hint: ordinal_or_hint, name };
    let export = match types.name_type() {
        pe::IMPORT_OBJECT_ORDINAL => ImportName::Ordinal(ordinal_or_hint),
        pe::IMPORT_OBJECT_NAME => named(&*symbol),
        pe::IMPORT_OBJECT_NAME_NO_PREFIX => named(strip(&symbol)),
        pe::IMPORT_OBJECT_NAME_UNDECORATE => named(strip(&symbol).split('@').next().unwrap_or_default()),
        pe::IMPORT_OBJECT_NAME_EXPORTAS => {
            export_as = text(strings.next().ok_or_else(short)?).map_err(|e| fail(name, e))?;
            named(&*export_as)
        }
        other => {
            return Err(fail(name, format_args!("import name type {other} of {symbol}, which blaze does not link")));
        }
    };
    let version = export.version();
    Ok((dll, Imported { name: symbol, version, kind }))
}

/// How the writer names an import in the DLL's export table, as `SharedSymbol::version` and
/// `Import::version` carry it on PE: `"<hint> <name>"` or `"#<ordinal>"`. Borrows the name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ImportName<'a> {
    Name { hint: u16, name: &'a str },
    Ordinal(u16),
}

impl<'a> ImportName<'a> {
    pub(crate) fn parse(symbol: &'a str, version: Option<&'a str>) -> Result<ImportName<'a>, String> {
        let Some(version) = version else {
            return Ok(ImportName::Name { hint: 0, name: symbol });
        };
        // alloc: error path
        let bad = || format!("the import {symbol} has no PE import name: {version:?}");
        if let Some(ordinal) = version.strip_prefix('#') {
            return ordinal.parse().map(ImportName::Ordinal).map_err(|_| bad());
        }
        let (hint, name) = version.split_once(' ').ok_or_else(bad)?;
        Ok(ImportName::Name { hint: hint.parse().map_err(|_| bad())?, name })
    }

    /// The version [`ImportName::parse`] reads back: one allocation of its exact size, the
    /// `Shared::versions` entry of an import.
    pub(crate) fn version(self) -> String {
        let mut digits = [0; 20];
        let (prefix, number, name) = match self {
            ImportName::Name { hint, name } => ("", decimal(u64::from(hint), &mut digits), Some(name)),
            ImportName::Ordinal(ordinal) => ("#", decimal(u64::from(ordinal), &mut digits), None),
        };
        let mut out = String::with_capacity(prefix.len() + number.len() + name.map_or(0, |it| it.len() + 1));
        out.push_str(prefix);
        out.push_str(number);
        if let Some(name) = name {
            out.push(' ');
            out.push_str(name);
        }
        out
    }
}

/// An archive's members and index, and the DLL each import descriptor member in it names. A
/// first pass over the member headers and the index sizes the three columns, so the read
/// allocates them once each (and two names per DLL an import library's descriptors name).
fn archive(name: Input<'_>, bytes: &[u8], whole: bool) -> Result<(Archive, Descriptors), String> {
    let file = ArchiveFile::parse(bytes).map_err(|e| fail(name, format_args!("not an archive: {e}")))?;
    if file.is_thin() {
        return Err(fail(name, "a thin archive, which link.exe does not read"));
    }
    match file.kind() {
        ArchiveKind::Coff | ArchiveKind::Gnu | ArchiveKind::Gnu64 | ArchiveKind::Unknown => {}
        other => return Err(fail(name, format_args!("a {other:?} archive, not a COFF one"))),
    }
    let (mut member_count, mut name_bytes) = (0, 0);
    for member in file.members() {
        member_count += 1;
        name_bytes += member.map_err(|e| fail(name, e))?.name().len();
    }
    let mut symbol_count = 0;
    if let Some(symbols) = file.symbols().map_err(|e| fail(name, e))? {
        for symbol in symbols {
            symbol_count += 1;
            name_bytes += symbol.map_err(|e| fail(name, e))?.name().len();
        }
    }
    let mut members = Vec::with_capacity(member_count);
    let mut names = Names::with_capacity(name_bytes);
    for member in file.members() {
        let member = member.map_err(|e| fail(name, e))?;
        let (offset, size) = member.file_range();
        // `object` takes a member's size from its header without bounding it by the archive.
        let range = usize::try_from(offset)
            .ok()
            .and_then(|start| Some(start..start.checked_add(usize::try_from(size).ok()?)?))
            .filter(|it| it.end <= bytes.len())
            .ok_or_else(|| fail(name, "a member past the end of the archive"))?;
        let member_name = names.push(&text(member.name()).map_err(|e| fail(name, e))?).map_err(|e| fail(name, e))?;
        members.push(Member { name: member_name, bytes: range });
    }
    let mut index = Vec::with_capacity(symbol_count);
    let mut descriptors = Descriptors::default();
    if let Some(symbols) = file.symbols().map_err(|e| fail(name, e))? {
        for symbol in symbols {
            let symbol = symbol.map_err(|e| fail(name, e))?;
            let offset = symbol.offset().0;
            let symbol_name = text(symbol.name()).map_err(|e| fail(name, e))?;
            // Members are in file order, so their headers' offsets ascend.
            let member = members
                .binary_search_by_key(&offset, |it| (it.bytes.start as u64).saturating_sub(MEMBER_HEADER))
                .map_err(|_| {
                    fail(name, format_args!("the index names {symbol_name} in a member at {offset:#x}, where none starts"))
                })? as u32;
            if symbol_name.starts_with("__IMPORT_DESCRIPTOR_") && !descriptors.contains_key(&*symbol_name) {
                let it = &members[member as usize];
                let input = Input { file: name.file, member: Some(names.at(it.name)) };
                let dll = descriptor_dll(input, &bytes[it.bytes.start..it.bytes.end])?;
                // alloc: once per DLL an import library imports from: the descriptor's name
                descriptors.insert((*symbol_name).to_owned(), dll);
            }
            index.push((names.push(&symbol_name).map_err(|e| fail(name, e))?, member));
        }
    }
    if index.is_empty() && !members.is_empty() && !whole {
        return Err(fail(
            name,
            "an archive with no symbol index, so no member of it can be taken for a symbol (index it \
             with lib.exe or llvm-ranlib)",
        ));
    }
    Ok((Archive { members, index, names }, descriptors))
}

/// The DLL an import descriptor member names: its `.idata$6`.
fn descriptor_dll(name: Input<'_>, data: &[u8]) -> Result<String, String> {
    let mut offset = 0;
    let header = pe::ImageFileHeader::parse(data, &mut offset)
        .map_err(|e| fail(name, format_args!("an import descriptor that is not a COFF object: {e}")))?;
    let sections = header.sections(data, offset).map_err(|e| fail(name, e))?;
    let symbols = header.symbols(data).map_err(|e| fail(name, e))?;
    for section in sections.iter() {
        if section.name(symbols.strings()).map_err(|e| fail(name, e))? != b".idata$6" {
            continue;
        }
        let bytes = section.coff_data(data).map_err(|()| fail(name, "a truncated .idata$6"))?;
        let dll = bytes.split(|it| *it == 0).next().unwrap_or_default();
        if !dll.is_empty() {
            return Ok(text(dll).map_err(|e| fail(name, e))?.into_owned());
        }
    }
    Err(fail(name, "an import descriptor naming no DLL (no .idata$6)"))
}

/// An import library linked as a shared library: every member an import of some DLL. The
/// imports are sorted by DLL and member (an umbrella library such as `windows.lib` has
/// thousands of members of hundreds of DLLs), each DLL built from its run, and a DLL's name is
/// copied once.
fn import_library(pe: &Pe, name: Input<'_>, bytes: &[u8]) -> Result<Shared, String> {
    if !bytes.starts_with(b"!<arch>\n") {
        if bytes.starts_with(b"MZ") {
            return Err(fail(
                name,
                "a DLL or executable; a PE image links against the DLL's import library (.lib), as \
                 link.exe does",
            ));
        }
        return Err(fail(name, "not an import library"));
    }
    let (archive, descriptors) = archive(name, bytes, true)?;
    // Every member parsed in place on every thread (each import's version made there), then
    // grouped by DLL on every thread: the first error in member order is the one given.
    let input_of = |it: &Member| Input { file: name.file, member: Some(archive.name(it.name)) };
    let mut parsed: Vec<Result<Option<(Cow<'_, str>, Imported<'_>)>, String>> =
        Vec::with_capacity(archive.members.len());
    par::extend(&mut parsed, &archive.members, |it| {
        let input = input_of(it);
        match member(pe, Dlls::Library(&descriptors), input, &bytes[it.bytes.start..it.bytes.end])? {
            Parsed::Import(dll, import) => Ok(Some((dll, import))),
            Parsed::Object(object) if object.sections.is_empty() => Ok(None),
            Parsed::Object(_) => Err(fail(
                input,
                "an object in a library linked as a DLL's import library; a library with \
                 objects is linked as an archive",
            )),
        }
    });
    // The members before the first that fails to parse are added; its error is given unless
    // adding one of them fails.
    let failed = match par::find_map_first(par::enumerate(&parsed), |(at, it)| it.is_err().then_some(at)) {
        Some(at) => {
            let error = parsed.swap_remove(at).err();
            parsed.truncate(at);
            error
        }
        None => None,
    };
    // Each DLL's imports together, in member order.
    let mut imports: Vec<(Cow<'_, str>, u32, Imported<'_>)> = par::filter_map(par::enumerate(parsed), |(at, it)| match it {
        Ok(Some((dll, import))) => Some((dll, at as u32, import)),
        _ => None,
    }); // alloc: once per library: its imports, to sort
    imports.sort_unstable_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
    // Each DLL's `Shared` built on its own thread, keyed by its first member; a failure keyed
    // by the member that failed.
    let mut bounds = Vec::new();
    let mut start = 0;
    for at in 1..imports.len() {
        if imports[at - 1].0 != imports[at].0 {
            bounds.push(at - start);
            start = at;
        }
    }
    if start < imports.len() {
        bounds.push(imports.len() - start);
    }
    let mut groups = Vec::with_capacity(bounds.len());
    let mut rest = imports.as_mut_slice();
    for len in bounds {
        let (group, tail) = rest.split_at_mut(len);
        groups.push(group);
        rest = tail;
    }
    let mut built: Vec<(u32, Result<Shared, (u32, String)>)> = par::map(groups, |run| {
        let first = run[0].1;
        // alloc: once per DLL: the name its `Shared` owns
        let mut dll = shared((*run[0].0).to_owned());
        for (_, at, import) in run.iter_mut() {
            let import = Imported {
                name: std::mem::take(&mut import.name),
                version: std::mem::take(&mut import.version),
                kind: import.kind,
            };
            if let Err(error) = add(&mut dll, input_of(&archive.members[*at as usize]), import) {
                return (first, Err((*at, error)));
            }
        }
        (first, Ok(dll))
    }); // alloc: once per library: one entry per DLL
    if let Some((_, error)) = built.iter_mut().filter_map(|(_, it)| it.as_mut().err()).min_by_key(|it| it.0) {
        return Err(std::mem::take(error));
    }
    if let Some(error) = failed {
        return Err(error);
    }
    // The DLLs in the order each is first met in the library, as one pass numbers them.
    built.sort_unstable_by_key(|it| it.0);
    let mut dlls = built.into_iter().filter_map(|(_, it)| it.ok());
    let Some(mut first) = dlls.next() else {
        return Err(fail(name, "an import library importing nothing"));
    };
    // alloc: once per library: its other DLLs
    first.more = dlls.collect();
    Ok(first)
}

/// The sections of a COFF object, by its section number less one, their names borrowed.
struct Sections<'d> {
    table: SectionTable<'d>,
    names: Vec<Cow<'d, str>>,
}

fn coff<'a, C: CoffHeader>(pe: &Pe, dlls: Dlls<'a>, name: Input<'_>, data: &'a [u8]) -> Result<Parsed<'a>, String> {
    let mut offset = 0;
    let header = C::parse(data, &mut offset).map_err(|e| fail(name, format_args!("not a COFF object: {e}")))?;
    match header.machine() {
        pe::IMAGE_FILE_MACHINE_AMD64 | pe::IMAGE_FILE_MACHINE_UNKNOWN => {}
        machine => {
            return Err(fail(name, format_args!("a COFF object for machine {machine:#x}, not x86-64")));
        }
    }
    let table = header.sections(data, offset).map_err(|e| fail(name, e))?;
    let symbols = header.symbols(data).map_err(|e| fail(name, e))?;
    let strings = symbols.strings();
    let mut names = Vec::with_capacity(table.len());
    let mut import = false;
    // What sizes the object's columns: its section names' bytes, `.drectve` bytes (the names of
    // `/ALTERNATENAME`), COMDAT sections and relocations.
    let (mut section_bytes, mut directive_bytes, mut comdats, mut relocations) = (0, 0, 0, 0);
    for section in table.iter() {
        let section_name = section.name(strings).map_err(|e| fail(name, e))?;
        import |= section_name.starts_with(b".idata$");
        section_bytes += section_name.len();
        if section_name == b".drectve" {
            directive_bytes += section.size_of_raw_data.get(LE) as usize;
        }
        comdats += usize::from(section.characteristics.get(LE).0 & pe::IMAGE_SCN_LNK_COMDAT.0 != 0);
        relocations += section.coff_relocations(data).map_or(0, <[_]>::len);
        names.push(text(section_name).map_err(|e| fail(name, e))?);
    }
    let sections = Sections { table, names };
    if import {
        return long_import(dlls, name, data, &sections, &symbols);
    }
    // A symbol's name is at most 8 bytes in its record or one string of the string table, whose
    // length is the word after the symbol table; a local COMDAT's group is named by the input
    // and the section's number. Each term is bounded by the input's size.
    let strings_at = header.pointer_to_symbol_table() as usize
        + symbols.len() * std::mem::size_of::<C::SymbolBytes>();
    let string_bytes = (u32_at(data, strings_at).unwrap_or(0) as usize).min(data.len());
    let capacity = string_bytes
        + 8 * symbols.len()
        + section_bytes
        + directive_bytes.min(data.len())
        + comdats * (name.len() + 12);
    let columns = Columns { names: capacity, comdats, relocations };
    Reader { pe, name, data, sections: &sections, symbols: &symbols, columns }.object().map(Parsed::Object)
}

/// What an object's columns are allocated with, from its section headers.
#[derive(Clone, Copy)]
struct Columns {
    /// Bytes of `Names`.
    names: usize,
    /// COMDAT sections: at most this many groups and grouped sections.
    comdats: usize,
    relocations: usize,
}

struct Reader<'a, 'd, C: CoffHeader> {
    pe: &'a Pe,
    name: Input<'a>,
    data: &'d [u8],
    sections: &'a Sections<'d>,
    symbols: &'a SymbolTable<'d, &'d [u8], C>,
    columns: Columns,
}

/// A section's first and second symbols (a COMDAT's section symbol, with the selection and an
/// associative section's parent, and its COMDAT symbol, naming the group): COFF indexes,
/// [`NONE`] for none.
#[derive(Clone, Copy)]
struct Marks {
    first: u32,
    second: u32,
}

impl Marks {
    const NONE: Marks = Marks { first: NONE, second: NONE };
}

fn index_of(coff: u32) -> Option<SymbolIndex> {
    (coff != NONE).then_some(SymbolIndex(coff as usize))
}

/// What a COFF symbol table is to the link, from one pass over it.
struct Read {
    symbols: Vec<Symbol>,
    /// The object's symbol for each COFF symbol table index, [`NONE`] for aux records and
    /// symbols no relocation can name: 4 bytes per COFF symbol, looked up by index. The
    /// worker's scratch buffer, so a warm worker's read doesn't allocate it.
    map: Lent<Vec<u32>>,
    /// Each section's [`Marks`]: 8 bytes per section.
    marks: Vec<Marks>,
}

impl<'d, C: CoffHeader> Reader<'_, 'd, C> {
    fn err(&self, what: impl fmt::Display) -> String {
        fail(self.name, what)
    }

    fn header(&self, index: usize) -> &'d pe::ImageSectionHeader {
        &self.sections.table.iter().as_slice()[index]
    }

    fn object(&self) -> Result<Object, String> {
        let mut object = Object {
            sections: Vec::with_capacity(self.sections.names.len()),
            fixups: Vec::with_capacity(self.columns.relocations),
            names: Names::with_capacity(self.columns.names),
            ..Object::default()
        };
        for (index, section_name) in self.sections.names.iter().enumerate() {
            let header = self.header(index);
            let flags = header.characteristics.get(LE).0;
            let section = self.section(&mut object.names, section_name, header, flags)?;
            object.sections.push(section);
        }
        let read = self.symbols(&mut object.names, &object.sections)?;
        self.groups(&mut object, &read)?;
        for (index, header) in self.sections.table.iter().enumerate() {
            if object.sections[index].what == SectionKind::Dropped {
                continue;
            }
            let start = self.run(object.fixups.len())?;
            self.fixups(index, header, &object.sections[index].data, &read, &object.names, &mut object.fixups)?;
            object.sections[index].fixups = start..self.run(object.fixups.len())?;
        }
        object.symbols = read.symbols;
        for (index, section_name) in self.sections.names.iter().enumerate() {
            if section_name == ".drectve" {
                let bytes = self.header(index).coff_data(self.data).map_err(|()| self.err("a truncated .drectve"))?;
                self.directives(bytes, &mut object)?;
            }
        }
        Ok(object)
    }

    /// The end of a run of one of the object's columns, as the contract's `u32` ranges hold it.
    fn run(&self, len: usize) -> Result<u32, String> {
        u32::try_from(len).map_err(|_| self.err("more than 4 Gi relocations or COMDAT members in one object"))
    }

    fn section(
        &self,
        names: &mut Names,
        section_name: &str,
        header: &pe::ImageSectionHeader,
        flags: u32,
    ) -> Result<Section, String> {
        let align = alignment(flags).ok_or_else(|| {
            self.err(format_args!(
                "section {section_name} has alignment bits {:#x}",
                flags & pe::IMAGE_SCN_ALIGN_MASK
            ))
        })?;
        let base = section_name.split('$').next().unwrap_or_default();
        let dropped = flags & (pe::IMAGE_SCN_LNK_REMOVE.0 | pe::IMAGE_SCN_LNK_INFO.0) != 0 || dropped_name(section_name);
        let zero = flags & pe::IMAGE_SCN_CNT_UNINITIALIZED_DATA.0 != 0;
        let data = if dropped {
            Data::Input(0..0)
        } else if zero {
            Data::Zero(header.size_of_raw_data.get(LE).into())
        } else {
            header
                .coff_data(self.data)
                .map_err(|()| self.err(format_args!("section {section_name} is past the end")))?;
            let start = u64::from(header.pointer_to_raw_data.get(LE));
            Data::Input(start..start + u64::from(header.size_of_raw_data.get(LE)))
        };
        let what = if dropped {
            SectionKind::Dropped
        } else if base == OBJ_PDATA {
            SectionKind::Unwind(super::named(OBJ_PDATA.into(), false, false))
        } else if base == ".xdata" {
            SectionKind::Unwind(Class::Rodata)
        } else {
            let write = flags & pe::IMAGE_SCN_MEM_WRITE.0 != 0;
            let exec = flags & (pe::IMAGE_SCN_MEM_EXECUTE.0 | pe::IMAGE_SCN_CNT_CODE.0) != 0;
            let (class, rank) =
                self.pe.section(section_name, write, exec, base == ".tls").map_err(|e| self.err(e))?;
            let class = match class {
                Class::Data if zero => Class::Bss,
                Class::Bss if !zero => Class::Data,
                class => class,
            };
            SectionKind::Alloc(class, rank)
        };
        let comdat = flags & pe::IMAGE_SCN_LNK_COMDAT.0 != 0;
        Ok(Section {
            name: names.push(section_name).map_err(|e| self.err(e))?,
            retain: what != SectionKind::Dropped && !comdat,
            what,
            align,
            data,
            fixups: 0..0,
        })
    }

    /// The object's symbols, each name pushed once: a section symbol named as its section takes
    /// the section's name. A weak external's default is resolved once every symbol is read.
    fn symbols(&self, names: &mut Names, sections: &[Section]) -> Result<Read, String> {
        let strings = self.symbols.strings();
        let count = self.sections.names.len();
        let mut map = scratch::lend_u32s();
        map.resize(self.symbols.len(), NONE);
        // alloc: once per object, 8 bytes per section
        let marks = vec![Marks::NONE; count];
        let mut read = Read { symbols: Vec::with_capacity(self.symbols.len()), map, marks };
        let mut weak = 0;
        // `(section, value, symbol)` of every symbol defined in a section but the section's
        // own, sorted once to size each by the next one.
        let mut defined: Vec<(u32, u64, u32)> = Vec::with_capacity(self.symbols.len());
        for (index, symbol) in self.symbols.iter() {
            let class = symbol.storage_class();
            let number = symbol.section_number().0;
            let coff_index = index.0 as u32;
            if number > 0 && number as usize <= count {
                let marks = &mut read.marks[number as usize - 1];
                if marks.first == NONE {
                    marks.first = coff_index;
                } else if marks.second == NONE {
                    marks.second = coff_index;
                }
            }
            let linkable = matches!(
                class,
                pe::IMAGE_SYM_CLASS_EXTERNAL
                    | pe::IMAGE_SYM_CLASS_STATIC
                    | pe::IMAGE_SYM_CLASS_LABEL
                    | pe::IMAGE_SYM_CLASS_WEAK_EXTERNAL
            );
            if !linkable || number == pe::IMAGE_SYM_DEBUG.0 {
                continue;
            }
            let symbol_name = text(symbol.name(strings).map_err(|e| self.err(e))?).map_err(|e| self.err(e))?;
            let value = u64::from(symbol.value());
            let function = symbol.derived_type() == pe::IMAGE_SYM_DTYPE_FUNCTION;
            let (def, kind) = if class == pe::IMAGE_SYM_CLASS_WEAK_EXTERNAL {
                if !symbol.has_aux_weak_external() {
                    return Err(self.err(format_args!("weak external {symbol_name} names no default")));
                }
                let aux = self.symbols.aux_weak_external(index).map_err(|e| self.err(e))?;
                let search = match aux.weak_search_type.get(LE) {
                    pe::IMAGE_WEAK_EXTERN_SEARCH_NOLIBRARY => false,
                    pe::IMAGE_WEAK_EXTERN_SEARCH_LIBRARY | pe::IMAGE_WEAK_EXTERN_SEARCH_ALIAS => true,
                    other => {
                        return Err(self.err(format_args!(
                            "weak external {symbol_name} has search type {other}, which blaze does not link"
                        )));
                    }
                };
                weak += 1;
                // The default's COFF index until every symbol is read.
                let default = aux.weak_default_sym_index.get(LE);
                (
                    SymbolDef::Fallback { symbol: default, search },
                    if function { SymbolKind::Function } else { SymbolKind::Other },
                )
            } else if number > 0 {
                let section = number as usize - 1;
                if section >= count {
                    return Err(self.err(format_args!(
                        "symbol {symbol_name} is in section {number}, which there isn't"
                    )));
                }
                let header = self.header(section);
                let flags = header.characteristics.get(LE).0;
                let (kind, size) = if symbol.has_aux_section() && value == 0 {
                    (SymbolKind::Section, u64::from(header.size_of_raw_data.get(LE)))
                } else if function || flags & (pe::IMAGE_SCN_MEM_EXECUTE.0 | pe::IMAGE_SCN_CNT_CODE.0) != 0 {
                    (SymbolKind::Function, 0)
                } else {
                    (SymbolKind::Data, 0)
                };
                if kind != SymbolKind::Section {
                    defined.push((section as u32, value, read.symbols.len() as u32));
                }
                (SymbolDef::Section { section: section as u32, value, size }, kind)
            } else if number == pe::IMAGE_SYM_ABSOLUTE.0 {
                (SymbolDef::Absolute(value), SymbolKind::Other)
            } else if number == pe::IMAGE_SYM_UNDEFINED.0 {
                let kind = if function { SymbolKind::Function } else { SymbolKind::Other };
                if value != 0 && class == pe::IMAGE_SYM_CLASS_EXTERNAL {
                    let align = value.next_power_of_two().min(32);
                    (SymbolDef::Common { size: value, align }, SymbolKind::Data)
                } else {
                    (SymbolDef::Undefined, kind)
                }
            } else {
                return Err(self.err(format_args!("symbol {symbol_name} has section number {number}")));
            };
            let binding = match class {
                pe::IMAGE_SYM_CLASS_EXTERNAL => Binding::Global,
                pe::IMAGE_SYM_CLASS_WEAK_EXTERNAL => Binding::Weak,
                _ => Binding::Local,
            };
            let own = match def {
                SymbolDef::Section { section, .. } if kind == SymbolKind::Section => sections
                    .get(section as usize)
                    .map(|it| it.name)
                    .filter(|it| names.at(*it) == symbol_name),
                _ => None,
            };
            let name = match own {
                Some(name) => name,
                None => names.push(&symbol_name).map_err(|e| self.err(e))?,
            };
            read.map[index.0] = read.symbols.len() as u32;
            read.symbols.push(Symbol { name, def, binding, kind, hidden: true });
        }
        // A symbol's size runs to the next greater value in its section, or the section's end.
        radix::sort_copy_by_key(&mut defined, |&(section, value, symbol)| {
            (u128::from(section) << 96) | (u128::from(value) << 32) | u128::from(symbol)
        });
        for run in defined.chunk_by(|a, b| a.0 == b.0) {
            let mut end = u64::from(self.header(run[0].0 as usize).size_of_raw_data.get(LE));
            let mut at = None;
            for &(_, value, symbol) in run.iter().rev() {
                if at != Some(value) {
                    if let Some(next) = at {
                        end = next;
                    }
                    at = Some(value);
                }
                if let SymbolDef::Section { size, .. } = &mut read.symbols[symbol as usize].def {
                    *size = end.saturating_sub(value);
                }
            }
        }
        if weak > 0 {
            for symbol in &mut read.symbols {
                let (Binding::Weak, SymbolDef::Fallback { symbol: default, .. }) = (symbol.binding, &mut symbol.def)
                else {
                    continue;
                };
                let coff = *default;
                *default = read.map.get(coff as usize).copied().filter(|it| *it != NONE).ok_or_else(|| {
                    self.err(format_args!(
                        "weak external {} defaults to symbol {coff}, which is not one",
                        names.at(symbol.name)
                    ))
                })?;
            }
        }
        Ok(read)
    }

    /// The section number an associative COMDAT section `index` names, if it is one.
    fn associated(&self, read: &Read, index: usize) -> Result<Option<u32>, String> {
        if self.header(index).characteristics.get(LE).0 & pe::IMAGE_SCN_LNK_COMDAT.0 == 0 {
            return Ok(None);
        }
        Ok(match self.comdat(read, index)? {
            (pe::IMAGE_COMDAT_SELECT_ASSOCIATIVE, parent) => Some(parent),
            _ => None,
        })
    }

    /// The selection and parent section number of COMDAT section `index`, from the definition
    /// record of its section symbol.
    fn comdat(&self, read: &Read, index: usize) -> Result<(pe::ComdatSelection, u32), String> {
        let aux = index_of(read.marks[index].first)
            .filter(|it| self.symbols.symbol(*it).is_ok_and(|it| it.has_aux_section()))
            .ok_or_else(|| {
                self.err(format_args!("COMDAT section {} has no section symbol", self.sections.names[index]))
            })?;
        let aux = self.symbols.aux_section(aux).map_err(|e| self.err(e))?;
        let mut parent = u32::from(aux.number.get(LE));
        if C::is_type_bigobj() {
            parent |= u32::from(aux.high_number.get(LE)) << 16;
        }
        Ok((aux.selection, parent))
    }

    /// The COMDAT groups with their sections in `object.grouped`, and which sections stay
    /// whatever refers to them. A global COMDAT's group is named by its leader symbol's name.
    fn groups(&self, object: &mut Object, read: &Read) -> Result<(), String> {
        if self.columns.comdats == 0 {
            return Ok(());
        }
        let strings = self.symbols.strings();
        let count = object.sections.len();
        // Each section's group, [`NONE`] for none: 4 bytes per section.
        let mut group_of = scratch::lend_u32s();
        group_of.resize(count, NONE);
        // `(group, section)`: each group's own section, then the associatives that join it;
        // sorted once by group (stably, so the group's own section stays first) into runs. A
        // COMDAT section is in at most one.
        let mut members: Vec<(u32, u32)> = Vec::with_capacity(self.columns.comdats);
        object.groups.reserve_exact(self.columns.comdats);
        // A local COMDAT's group name, made in the worker's string buffer.
        let mut local = scratch::lend_string();
        for index in 0..count {
            if self.header(index).characteristics.get(LE).0 & pe::IMAGE_SCN_LNK_COMDAT.0 == 0
                || object.sections[index].what == SectionKind::Dropped
            {
                continue;
            }
            let largest = match self.comdat(read, index)?.0 {
                pe::IMAGE_COMDAT_SELECT_ASSOCIATIVE => {
                    group_of[index] = ASSOCIATIVE;
                    continue;
                }
                pe::IMAGE_COMDAT_SELECT_NODUPLICATES
                | pe::IMAGE_COMDAT_SELECT_ANY
                | pe::IMAGE_COMDAT_SELECT_SAME_SIZE
                | pe::IMAGE_COMDAT_SELECT_EXACT_MATCH => false,
                pe::IMAGE_COMDAT_SELECT_LARGEST => true,
                other => {
                    return Err(self.err(format_args!(
                        "COMDAT section {} has selection {other}, which blaze does not link",
                        self.sections.names[index]
                    )));
                }
            };
            let second = read.marks[index].second;
            let leader = index_of(second).and_then(|it| self.symbols.symbol(it).ok()).ok_or_else(|| {
                self.err(format_args!("COMDAT section {} has no COMDAT symbol", self.sections.names[index]))
            })?;
            let name = if leader.storage_class() == pe::IMAGE_SYM_CLASS_EXTERNAL {
                // An external symbol in a section is always one of the object's.
                match read.map.get(second as usize).copied().filter(|it| *it != NONE) {
                    Some(symbol) => read.symbols[symbol as usize].name,
                    None => object
                        .names
                        .push(&text(leader.name(strings).map_err(|e| self.err(e))?).map_err(|e| self.err(e))?)
                        .map_err(|e| self.err(e))?,
                }
            } else {
                local.clear();
                write!(local, "\0{}\0{index}", self.name).map_err(|e| self.err(e))?;
                object.names.push(&local).map_err(|e| self.err(e))?
            };
            let group = object.groups.len() as u32;
            group_of[index] = group;
            members.push((group, index as u32));
            object.groups.push(Group { name, sections: 0..0, largest });
        }
        for index in 0..count {
            if group_of[index] != ASSOCIATIVE {
                continue;
            }
            let parent = self.comdat(read, index)?.1 as usize;
            let mut root = parent;
            let mut steps = 0;
            loop {
                if root == 0 || root > count {
                    return Err(self.err(format_args!(
                        "associative COMDAT section {} names section {root}, which there isn't",
                        self.sections.names[index]
                    )));
                }
                match self.associated(read, root - 1)? {
                    Some(next) if steps < count => {
                        root = next as usize;
                        steps += 1;
                    }
                    Some(_) => {
                        return Err(self.err(format_args!(
                            "associative COMDAT section {} is in a cycle",
                            self.sections.names[index]
                        )));
                    }
                    None => break,
                }
            }
            // lld-link: an associative section is kept when its parent is, and keeps
            // nothing by itself.
            let tie = Tie { section: index as u32, to: (parent - 1) as u32, ordered: false };
            match group_of[root - 1] {
                NONE if self.header(root - 1).characteristics.get(LE).0 & pe::IMAGE_SCN_LNK_COMDAT.0 == 0 => {
                    object.sections[index].retain = true;
                    object.ties.push(tie);
                }
                NONE | ASSOCIATIVE => {}
                group => {
                    members.push((group, index as u32));
                    object.ties.push(tie);
                }
            }
        }
        radix::sort_copy_by_key(&mut members, |it| it.0);
        object.grouped.reserve_exact(members.len());
        for run in members.chunk_by(|a, b| a.0 == b.0) {
            let start = self.run(object.grouped.len())?;
            object.grouped.extend(run.iter().map(|it| it.1));
            object.groups[run[0].0 as usize].sections = start..self.run(object.grouped.len())?;
        }
        Ok(())
    }

    /// Appends the section's relocations to `fixups` (the object's column, sized for every
    /// section's): a `match` on the type per relocation, the implicit addend read from the
    /// section's bytes in the input, no allocation.
    fn fixups(
        &self,
        index: usize,
        header: &pe::ImageSectionHeader,
        data: &Data,
        read: &Read,
        names: &Names,
        fixups: &mut Vec<NativeFixup>,
    ) -> Result<(), String> {
        let relocations = header.coff_relocations(self.data).map_err(|e| self.err(e))?;
        let section_name = &self.sections.names[index];
        let bytes = match data.bytes(self.data).map_err(|e| self.err(format_args!("section {section_name}: {e}")))? {
            Some(bytes) => bytes,
            None if relocations.is_empty() => return Ok(()),
            None => {
                return Err(self.err(format_args!("relocations in {section_name}, which has no bytes")));
            }
        };
        fixups.reserve(relocations.len());
        for relocation in relocations {
            let at = relocation.virtual_address.get(LE) as usize;
            let typ = relocation.typ.get(LE);
            if typ == pe::IMAGE_REL_AMD64_ABSOLUTE {
                continue;
            }
            let index = relocation.symbol_table_index.get(LE) as usize;
            let symbol = read.map.get(index).copied().filter(|it| *it != NONE).ok_or_else(|| {
                self.err(format_args!(
                    "a relocation in {section_name} at {at:#x} names symbol {index}, which is not one \
                     a relocation can name"
                ))
            })?;
            let target = &read.symbols[symbol as usize];
            let branch = matches!(target.kind, SymbolKind::Function)
                || matches!(target.def, SymbolDef::Undefined | SymbolDef::Fallback { .. });
            let short = || self.err(format_args!("a relocation in {section_name} at {at:#x} past its end"));
            let (apply, addend) = match typ {
                pe::IMAGE_REL_AMD64_ADDR64 => (Apply::Abs64, u64_at(bytes, at).ok_or_else(short)? as i64),
                pe::IMAGE_REL_AMD64_ADDR32 => (Apply::Abs32, i64::from(u32_at(bytes, at).ok_or_else(short)?)),
                pe::IMAGE_REL_AMD64_ADDR32NB => {
                    (Apply::ImageRel32, i64::from(u32_at(bytes, at).ok_or_else(short)? as i32))
                }
                rel32 if (pe::IMAGE_REL_AMD64_REL32..=pe::IMAGE_REL_AMD64_REL32_5).contains(&rel32) => {
                    let implicit = i64::from(u32_at(bytes, at).ok_or_else(short)? as i32);
                    let past = 4 + i64::from(rel32.0 - pe::IMAGE_REL_AMD64_REL32.0);
                    (if branch { Apply::X86Branch32 } else { Apply::Rel32 }, implicit - past)
                }
                pe::IMAGE_REL_AMD64_SECTION => (Apply::Section16, i64::from(u16_at(bytes, at).ok_or_else(short)?)),
                pe::IMAGE_REL_AMD64_SECREL => {
                    (Apply::SecRel32, i64::from(u32_at(bytes, at).ok_or_else(short)? as i32))
                }
                other => {
                    return Err(self.err(format_args!(
                        "relocation type {other:#x} ({}) in {section_name} at {at:#x} against {}, which \
                         blaze does not link",
                        relocation_name(other),
                        names.at(target.name)
                    )));
                }
            };
            fixups.push(NativeFixup { at: at as u64, apply, symbol, addend, minus: None });
        }
        Ok(())
    }

    /// A `.drectve`'s options, its words borrowed from the input but for quoted ones, which are
    /// made in one buffer reused for each.
    fn directives(&self, bytes: &[u8], object: &mut Object) -> Result<(), String> {
        let all = text(bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes)).map_err(|e| self.err(e))?;
        scratch::with_string(|quoted| words(&all, quoted, |word| self.directive(word, object)))
    }

    fn directive(&self, word: &str, object: &mut Object) -> Result<(), String> {
        let Some(option) = word.strip_prefix(['/', '-']) else {
            return Err(self.err(format_args!("the .drectve word {word:?} is not an option")));
        };
        let (option, arg) = match option.split_once(':') {
            Some((option, arg)) => (option, Some(arg)),
            None => (option, None),
        };
        // The option's name lowercased on the stack: no known one is longer.
        let mut buf = [0u8; 16];
        let key = match buf.get_mut(..option.len()) {
            Some(key) => {
                key.copy_from_slice(option.as_bytes());
                key.make_ascii_lowercase();
                &*key
            }
            None => &b""[..],
        };
        let need = || self.err(format_args!("the .drectve option /{option} needs an argument"));
        match key {
            b"defaultlib" => {
                let (stem, extension) = default_library(arg.ok_or_else(need)?);
                if !object.libraries.iter().any(|it| joins(it, stem, extension)) {
                    object.libraries.push(join(stem, extension));
                }
            }
            // alloc: once per directive of the file: `Directive` holds owned names
            b"include" => object.directives.push(Directive::Include(arg.ok_or_else(need)?.to_owned())),
            b"alternatename" => {
                let arg = arg.ok_or_else(need)?;
                let (from, to) = arg
                    .split_once('=')
                    .ok_or_else(|| self.err(format_args!("/ALTERNATENAME:{arg} is not from=to")))?;
                let from = object.names.push(from).map_err(|e| self.err(e))?;
                let to = object.names.push(to).map_err(|e| self.err(e))?;
                alternate(object, from, to);
            }
            b"export" => object.directives.push(self.export(arg.ok_or_else(need)?)?),
            b"failifmismatch" => {
                let arg = arg.ok_or_else(need)?;
                let (key, value) = arg
                    .split_once('=')
                    .ok_or_else(|| self.err(format_args!("/FAILIFMISMATCH:{arg} is not key=value")))?;
                // alloc: once per directive of the file: `Directive` holds owned names
                object.directives.push(Directive::Mismatch { key: key.to_owned(), value: value.to_owned() });
            }
            b"nodefaultlib" => {
                let library = arg.map(|it| {
                    let (stem, extension) = default_library(it);
                    join(stem, extension)
                });
                object.directives.push(Directive::NoDefaultLib(library));
            }
            // These change nothing a program observes: section merging (every section of
            // a protection is laid out together already), incremental-link padding, the
            // control-flow-guard tables of /guard:cf (off), a check that a library is not
            // linked.
            b"merge" | b"editandcontinue" | b"guardsym" | b"disallowlib" => {}
            _ => {
                return Err(self.err(format_args!(
                    "the .drectve option {word}, which blaze does not link (it would change the image \
                     as link.exe makes it)"
                )));
            }
        }
        Ok(())
    }

    /// `/EXPORT:name[=internal][,@ordinal[,NONAME]][,DATA][,PRIVATE]`.
    fn export(&self, arg: &str) -> Result<Directive, String> {
        let mut parts = arg.split(',');
        let first = parts.next().unwrap_or_default();
        let (name, symbol) = first.split_once('=').unwrap_or((first, first));
        if name.is_empty() || symbol.contains('.') {
            return Err(self.err(format_args!("/EXPORT:{arg}: a forwarder or no name, which blaze does not link")));
        }
        let mut data = false;
        for part in parts {
            if part.eq_ignore_ascii_case("DATA") {
                data = true;
            } else if !part.eq_ignore_ascii_case("PRIVATE") {
                return Err(self.err(format_args!(
                    "/EXPORT:{arg}: exports by ordinal (@n, NONAME), which blaze does not link"
                )));
            }
        }
        // alloc: once per directive of the file: `Directive` holds owned names
        Ok(Directive::Export { name: name.to_owned(), symbol: symbol.to_owned(), data })
    }
}

/// `/ALTERNATENAME:from=to`: `from` falls back to `to`, the symbol of that name the object
/// defines or refers to (a new undefined one when it has none).
fn alternate(object: &mut Object, from: Name, to: Name) {
    let names = &object.names;
    let found =
        object.symbols.iter().position(|it| it.binding != Binding::Local && names.at(it.name) == names.at(to));
    let target = match found {
        Some(at) => at as u32,
        None => {
            object.symbols.push(Symbol {
                name: to,
                def: SymbolDef::Undefined,
                binding: Binding::Global,
                kind: SymbolKind::Other,
                hidden: true,
            });
            object.symbols.len() as u32 - 1
        }
    };
    object.symbols.push(Symbol {
        name: from,
        def: SymbolDef::Fallback { symbol: target, search: false },
        binding: Binding::Global,
        kind: SymbolKind::Other,
        hidden: true,
    });
}

/// A COFF section's alignment: 16 when it states none, as the format says.
pub(super) fn alignment(flags: u32) -> Option<u64> {
    match (flags & pe::IMAGE_SCN_ALIGN_MASK) >> 20 {
        0 => Some(16),
        bits @ 1..=14 => Some(1 << (bits - 1)),
        _ => None,
    }
}

/// Sections no image keeps: debug info (CodeView and DWARF; no PDB is written), the
/// control-flow-guard tables of `/guard:cf` (off), LLVM's address-significance and call-graph
/// tables, embedded bitcode, and the volatile-metadata table link.exe discards.
fn dropped_name(name: &str) -> bool {
    let base = name.split('$').next().unwrap_or_default();
    name.starts_with(".debug")
        || matches!(base, ".gfids" | ".giats" | ".gljmp" | ".gehcont" | ".sxdata" | ".voltbl" | ".chks64")
        || matches!(name, ".llvm_addrsig" | ".llvm.call-graph-profile" | ".llvmbc" | ".llvmcmd")
}

/// A `/DEFAULTLIB` name as link.exe takes it: `.lib` added when it has no extension. The name
/// and what is added to it.
fn default_library(name: &str) -> (&str, &'static str) {
    let file = name.rsplit(['/', '\\']).next().unwrap_or(name);
    (name, if file.contains('.') { "" } else { ".lib" })
}

/// The default libraries the members of the COFF archive `bytes` ask for, for finding what a
/// link takes them with: each member's `/DEFAULTLIB`s less its own `/NODEFAULTLIB`s (none after
/// a bare one), as `(member, library)` in member order, each library once (the first member
/// asking) and named as link.exe names it (`.lib` added when it has no extension). Only the
/// section table and `.drectve` are read. An import member asks for nothing, and an anonymous
/// object built for LTCG or `/clr` carries its directives in what blaze does not read; the
/// engine refuses such a member if it is ever taken.
pub(crate) fn asked_libraries(name: &str, bytes: &[u8]) -> Result<Vec<(String, String)>, String> {
    let input = Input { file: name, member: None };
    let file = ArchiveFile::parse(bytes).map_err(|e| fail(input, format_args!("not an archive: {e}")))?;
    if file.is_thin() {
        return Err(fail(input, "a thin archive, which link.exe does not read"));
    }
    // alloc: once per library searched for its default libraries (setup, kept per process)
    let mut out: Vec<(String, String)> = Vec::new();
    let mut quoted = scratch::lend_string();
    for member in file.members() {
        let member = member.map_err(|e| fail(input, e))?;
        let member_name = text(member.name()).map_err(|e| fail(input, e))?;
        let at = Input { file: name, member: Some(&member_name) };
        let data = member.data(bytes).map_err(|e| fail(at, e))?;
        let drectve = match (u16_at(data, 0).map(pe::Machine), u16_at(data, 2), u16_at(data, 4)) {
            (Some(pe::IMAGE_FILE_MACHINE_UNKNOWN), Some(0xffff), Some(version)) => {
                if version >= 2 && data.get(12..28) == Some(&BIGOBJ[..]) {
                    drectve::<pe::AnonObjectHeaderBigobj>(at, data)?
                } else {
                    None
                }
            }
            _ => drectve::<pe::ImageFileHeader>(at, data)?,
        };
        let Some(drectve) = drectve else { continue };
        let all = text(drectve.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(drectve)).map_err(|e| fail(at, e))?;
        let start = out.len();
        let mut none = false;
        // alloc: none unless a member takes out more than four of its own libraries
        let mut left_out: smallvec::SmallVec<[String; 4]> = smallvec::SmallVec::new();
        words(&all, &mut quoted, |word| -> Result<(), String> {
            let Some(option) = word.strip_prefix(['/', '-']) else { return Ok(()) };
            let (option, arg) = match option.split_once(':') {
                Some((option, arg)) => (option, Some(arg)),
                None => (option, None),
            };
            if option.eq_ignore_ascii_case("defaultlib") {
                let arg = arg.ok_or_else(|| fail(at, "the .drectve option /DEFAULTLIB needs an argument"))?;
                let (stem, extension) = default_library(arg);
                if !out.iter().any(|it| joins_ignoring_case(&it.1, stem, extension)) {
                    // alloc: once per default library an archive's members name (setup)
                    out.push((member_name.clone().into_owned(), join(stem, extension)));
                }
            } else if option.eq_ignore_ascii_case("nodefaultlib") {
                match arg {
                    None => none = true,
                    Some(arg) => {
                        let (stem, extension) = default_library(arg);
                        // alloc: once per `/NODEFAULTLIB:lib` of a member (setup)
                        left_out.push(join(stem, extension));
                    }
                }
            }
            Ok(())
        })?;
        if none {
            out.truncate(start);
        } else if !left_out.is_empty() {
            let mut kept = start;
            for at in start..out.len() {
                if !left_out.iter().any(|gone| gone.eq_ignore_ascii_case(&out[at].1)) {
                    out.swap(kept, at);
                    kept += 1;
                }
            }
            out.truncate(kept);
        }
    }
    Ok(out)
}

/// The `.drectve` of the COFF object `data`, if it has one.
fn drectve<'d, C: CoffHeader>(name: Input<'_>, data: &'d [u8]) -> Result<Option<&'d [u8]>, String> {
    let mut offset = 0;
    let header = C::parse(data, &mut offset).map_err(|e| fail(name, format_args!("not a COFF object: {e}")))?;
    let sections = header.sections(data, offset).map_err(|e| fail(name, e))?;
    let symbols = header.symbols(data).map_err(|e| fail(name, e))?;
    for section in sections.iter() {
        if section.name(symbols.strings()).map_err(|e| fail(name, e))? == b".drectve" {
            return section.coff_data(data).map(Some).map_err(|()| fail(name, "a truncated .drectve"));
        }
    }
    Ok(None)
}

/// Whether `it` is `stem` then `extension`, ASCII case ignored.
fn joins_ignoring_case(it: &str, stem: &str, extension: &str) -> bool {
    it.len() == stem.len() + extension.len()
        && it.is_char_boundary(stem.len())
        && it[..stem.len()].eq_ignore_ascii_case(stem)
        && it[stem.len()..].eq_ignore_ascii_case(extension)
}

/// Whether `it` is `stem` then `extension`.
fn joins(it: &str, stem: &str, extension: &str) -> bool {
    it.len() == stem.len() + extension.len() && it.starts_with(stem) && it.ends_with(extension)
}

/// `stem` then `extension`, in one allocation of their size: an `Object::libraries` entry.
fn join(stem: &str, extension: &str) -> String {
    let mut out = String::with_capacity(stem.len() + extension.len());
    out.push_str(stem);
    out.push_str(extension);
    out
}

/// Hands each word of a `.drectve` to `each`, in order: split at spaces outside quotes, the
/// quotes (and NULs outside them) removed. A word with neither is borrowed from `text`; one
/// with them is made in `buf`, which is reused for every such word.
pub(super) fn words<E>(
    text: &str,
    buf: &mut String,
    mut each: impl FnMut(&str) -> Result<(), E>,
) -> Result<(), E> {
    let mut start = None;
    let mut made = false;
    let mut quoted = false;
    for (at, c) in text.char_indices() {
        let kept = match c {
            '"' => {
                quoted = !quoted;
                false
            }
            c if c.is_whitespace() && !quoted => {
                if let Some(from) = start.take() {
                    each(if made { buf.as_str() } else { &text[from..at] })?;
                    made = false;
                }
                continue;
            }
            '\0' if !quoted && start.is_none() => continue,
            '\0' if !quoted => false,
            _ => true,
        };
        let from = *start.get_or_insert(at);
        match (made, kept) {
            (true, true) => buf.push(c),
            (false, false) => {
                buf.clear();
                buf.push_str(&text[from..at]);
                made = true;
            }
            (true, false) | (false, true) => {}
        }
    }
    match start {
        Some(from) => each(if made { buf.as_str() } else { &text[from..] }),
        None => Ok(()),
    }
}

fn relocation_name(typ: pe::RelocationType) -> &'static str {
    match typ {
        pe::IMAGE_REL_AMD64_SECREL7 => "SECREL7",
        pe::IMAGE_REL_AMD64_TOKEN => "TOKEN",
        pe::IMAGE_REL_AMD64_SREL32 => "SREL32",
        pe::IMAGE_REL_AMD64_PAIR => "PAIR",
        pe::IMAGE_REL_AMD64_SSPAN32 => "SSPAN32",
        pe::IMAGE_REL_AMD64_EHANDLER => "EHANDLER",
        pe::IMAGE_REL_AMD64_IMPORT_BR => "IMPORT_BR",
        pe::IMAGE_REL_AMD64_IMPORT_CALL => "IMPORT_CALL",
        typ if (pe::IMAGE_REL_AMD64_CFG_BR..=pe::IMAGE_REL_AMD64_INDIR_CALL).contains(&typ) => {
            "a control-flow-guard one"
        }
        typ if (pe::IMAGE_REL_AMD64_INDIR_BR_SWITCHTABLE_FIRST..=pe::IMAGE_REL_AMD64_INDIR_BR_SWITCHTABLE_LAST)
            .contains(&typ) =>
        {
            "a switch-table one"
        }
        _ => "unknown",
    }
}

/// A long-format import member: an object with `.idata$` sections. One importing a function
/// or datum is read as that import; the descriptor, null descriptor and null thunk members
/// hold the import tables the writer makes itself, so they are read as empty objects. Names
/// are compared as the input's bytes and borrowed from it.
fn long_import<'a, C: CoffHeader>(
    dlls: Dlls<'a>,
    name: Input<'_>,
    data: &'a [u8],
    sections: &Sections<'a>,
    symbols: &SymbolTable<'a, &'a [u8], C>,
) -> Result<Parsed<'a>, String> {
    let strings = symbols.strings();
    let headers = sections.table.iter().as_slice();
    let mut imp: Option<(&'a [u8], usize)> = None;
    let mut descriptor: Option<&'a [u8]> = None;
    for (_, symbol) in symbols.iter() {
        if symbol.storage_class() != pe::IMAGE_SYM_CLASS_EXTERNAL {
            continue;
        }
        let symbol_name = symbol.name(strings).map_err(|e| fail(name, e))?;
        let number = symbol.section_number().0;
        if number > 0 {
            if symbol_name.starts_with(b"__IMPORT_DESCRIPTOR_")
                || symbol_name == b"__NULL_IMPORT_DESCRIPTOR"
                || symbol_name.ends_with(b"_NULL_THUNK_DATA")
            {
                return Ok(Parsed::Object(Object::default()));
            }
            let section = number as usize - 1;
            if let Some(stripped) = symbol_name.strip_prefix(b"__imp_")
                && sections.names.get(section).is_some_and(|it| it.starts_with(".idata$5"))
            {
                imp = Some((stripped, section));
            }
        } else if number == pe::IMAGE_SYM_UNDEFINED.0 && symbol_name.starts_with(b"__IMPORT_DESCRIPTOR_") {
            descriptor = Some(symbol_name);
        }
    }
    let Some((stripped, section)) = imp else {
        return Err(fail(
            name,
            "an object with .idata$ sections that defines no __imp_ symbol in .idata$5: the import \
             tables are the linker's",
        ));
    };
    let symbol = text(stripped).map_err(|e| fail(name, e))?;
    let descriptor = text(descriptor.ok_or_else(|| {
        fail(name, format_args!("the import member of {symbol} names no __IMPORT_DESCRIPTOR_ (a GNU dlltool library?)"))
    })?)
    .map_err(|e| fail(name, e))?;
    let dll = dlls.get(&descriptor).ok_or_else(|| {
        fail(name, format_args!("no archive read so far has {descriptor}, the DLL {symbol} is from"))
    })?;
    let header = headers.get(section).ok_or_else(|| fail(name, "no .idata$5"))?;
    let thunk = header.coff_data(data).map_err(|()| fail(name, "a truncated .idata$5"))?;
    let relocations = header.coff_relocations(data).map_err(|e| fail(name, e))?;
    let version = match relocations.first() {
        Some(relocation) => {
            let target = symbols
                .symbol(SymbolIndex(relocation.symbol_table_index.get(LE) as usize))
                .map_err(|e| fail(name, e))?;
            let number = target.section_number().0;
            let names = (number > 0)
                .then(|| headers.get(number as usize - 1))
                .flatten()
                .ok_or_else(|| fail(name, format_args!("the import name of {symbol} is in no section")))?;
            let bytes = names.coff_data(data).map_err(|()| fail(name, "a truncated .idata$6"))?;
            let addend = u64_at(thunk, 0).ok_or_else(|| fail(name, "a truncated .idata$5"))?;
            let at = usize::try_from(addend)
                .ok()
                .and_then(|thunk| (target.value() as usize).checked_add(thunk))
                .ok_or_else(|| fail(name, "an import name past the end of .idata$6"))?;
            let hint = u16_at(bytes, at).ok_or_else(|| fail(name, "a truncated .idata$6"))?;
            let export = bytes[at + 2..].split(|it| *it == 0).next().unwrap_or_default();
            ImportName::Name { hint, name: &text(export).map_err(|e| fail(name, e))? }.version()
        }
        None => {
            let value = u64_at(thunk, 0).ok_or_else(|| fail(name, "a truncated .idata$5"))?;
            if value & pe::IMAGE_ORDINAL_FLAG64 == 0 {
                return Err(fail(name, format_args!("the import of {symbol} has neither a name nor an ordinal")));
            }
            ImportName::Ordinal(value as u16).version()
        }
    };
    // A function when the member defines the symbol (its thunk) in code.
    let code = symbols.iter().any(|(_, it)| {
        let number = it.section_number().0;
        it.storage_class() == pe::IMAGE_SYM_CLASS_EXTERNAL
            && number > 0
            && it.name(strings).is_ok_and(|it| it == stripped && !it.starts_with(b"__imp_"))
            && headers.get(number as usize - 1).is_some_and(|header| {
                header.characteristics.get(LE).0 & (pe::IMAGE_SCN_MEM_EXECUTE.0 | pe::IMAGE_SCN_CNT_CODE.0) != 0
            })
    });
    let kind = if code { SymbolKind::Function } else { SymbolKind::Data };
    Ok(Parsed::Import(dll, Imported { name: symbol, version, kind }))
}
