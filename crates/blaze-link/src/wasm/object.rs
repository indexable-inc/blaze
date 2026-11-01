//! Reads wasm objects as LLVM writes them (wasi-libc's members, `crt1-command.o`, objects of
//! C dependencies) and archives of them, into the contract's [`Object`] and [`Archive`].
//!
//! An object's function becomes a [`Class::Text`] piece in the form blaze's codegen gives its
//! own ([`blaze_codegen::wasm::Code`]: the header, then the body), so the writer reads every code piece one way.
//! Its data segments become pieces by their names (`.rodata.*`, `.data.*`, `.bss.*`, or a
//! named section for `__start_`/`__stop_`), its custom sections custom-section pieces, its
//! init functions [`Class::InitArray`] pieces of their priority holding the function's slot.
//! The format: <https://github.com/WebAssembly/tool-conventions/blob/main/Linking.md>.

use std::collections::hash_map::Entry;
use std::ops::Range;

use blaze_hash::HashMap;
use blaze_ids::radix;

use super::code::{self, Parts, Sig};
use super::reader::{self, Reader, Wasm};
use super::reloc::{self, Kind};
use crate::model::{
    Apply, Archive, Binding, Class, Data, Group, Member, Name, Names, NativeFile, NativeFixup,
    Object, Section, SectionKind, Symbol, SymbolDef, SymbolKind, WasmImport,
};
use crate::output::NativeKind;

mod id {
    pub(super) const CUSTOM: u8 = 0;
    pub(super) const TYPE: u8 = 1;
    pub(super) const IMPORT: u8 = 2;
    pub(super) const FUNCTION: u8 = 3;
    pub(super) const TABLE: u8 = 4;
    pub(super) const MEMORY: u8 = 5;
    pub(super) const GLOBAL: u8 = 6;
    pub(super) const EXPORT: u8 = 7;
    pub(super) const START: u8 = 8;
    pub(super) const ELEMENT: u8 = 9;
    pub(super) const CODE: u8 = 10;
    pub(super) const DATA: u8 = 11;
    pub(super) const DATA_COUNT: u8 = 12;
    pub(super) const TAG: u8 = 13;
}

/// The subsections of the `linking` section.
const SEGMENT_INFO: u8 = 5;
const INIT_FUNCS: u8 = 6;
const COMDAT_INFO: u8 = 7;
const SYMBOL_TABLE: u8 = 8;
const LINKING_VERSION: u32 = 2;

const SYM_FUNCTION: u8 = 0;
const SYM_DATA: u8 = 1;
const SYM_GLOBAL: u8 = 2;
const SYM_SECTION: u8 = 3;
const SYM_TAG: u8 = 4;
const SYM_TABLE: u8 = 5;

const BINDING_WEAK: u32 = 0x1;
const BINDING_LOCAL: u32 = 0x2;
const VISIBILITY_HIDDEN: u32 = 0x4;
const UNDEFINED: u32 = 0x10;
const EXPORTED: u32 = 0x20;
const EXPLICIT_NAME: u32 = 0x40;
const NO_STRIP: u32 = 0x80;
const TLS: u32 = 0x100;
const ABSOLUTE: u32 = 0x200;

const SEGMENT_TLS: u32 = 0x2;
const SEGMENT_RETAIN: u32 = 0x4;

const COMDAT_DATA: u8 = 0;
const COMDAT_FUNCTION: u8 = 1;
const COMDAT_SECTION: u8 = 5;

pub(super) fn read(name: &str, bytes: &[u8], kind: &NativeKind) -> Result<NativeFile, String> {
    match kind {
        NativeKind::Object => {
            // alloc: error path
            object(bytes).map(NativeFile::Object).map_err(|e| format!("{name}: {e}"))
        }
        NativeKind::Archive { whole } => {
            // alloc: error path
            archive(bytes, *whole).map(NativeFile::Archive).map_err(|e| format!("{name}: {e}"))
        }
        // alloc: error path
        NativeKind::Shared { .. } => Err(format!(
            "{name}: a shared library, and a wasm32 module links none (wasm-ld takes them only \
             with -pie or -shared, which rustc's wasm targets don't use)"
        )),
    }
}

fn archive(bytes: &[u8], whole: bool) -> Result<Archive, String> {
    use ::object::read::archive::ArchiveFile;

    // alloc: error path
    let file = ArchiveFile::parse(bytes).map_err(|e| format!("an archive: {e}"))?;
    if file.is_thin() {
        // alloc: error path
        return Err("a thin archive, whose members are other files".to_owned());
    }
    // A first walk of the headers counts the members and the index and their names, so every
    // column is one exact allocation. The index's names can't be more bytes than the archive
    // (a BSD index names each string once however many entries name it).
    let (mut count, mut name_bytes) = (0, 0);
    for member in file.members().flatten() {
        count += 1;
        name_bytes += member.name().len();
    }
    let (mut entries, mut entry_bytes) = (0, 0);
    if let Ok(Some(index)) = file.symbols() {
        for symbol in index.flatten() {
            entries += 1;
            entry_bytes += symbol.name().len();
        }
    }
    let mut members = Vec::with_capacity(count);
    let mut names = Names::with_capacity(name_bytes + entry_bytes.min(bytes.len()));
    // Where each member's bytes start, in file order and so sorted: found by binary search.
    let mut by_offset: Vec<u64> = Vec::with_capacity(count);
    for member in file.members() {
        // alloc: error path
        let member = member.map_err(|e| format!("an archive member: {e}"))?;
        let (offset, size) = member.file_range();
        // alloc: borrowed, unless the name is not UTF-8
        let member_name = String::from_utf8_lossy(member.name());
        // `object` takes a member's size from its header without bounding it by the archive.
        let range = offset
            .checked_add(size)
            .and_then(|end| Some(usize::try_from(offset).ok()?..usize::try_from(end).ok()?))
            .filter(|it| it.end <= bytes.len())
            .ok_or_else(|| {
                // alloc: error path
                format!(
                    "the member `{member_name}` of {size} bytes at {offset} reaches past the \
                     archive's {} bytes",
                    bytes.len()
                )
            })?;
        by_offset.push(offset);
        let name = names.push(&member_name)?;
        members.push(Member { name, bytes: range });
    }
    let mut index = Vec::with_capacity(entries);
    // alloc: error path
    match file.symbols().map_err(|e| format!("the archive's index: {e}"))? {
        Some(symbols) => {
            // A BSD `__.SYMDEF` names its strings by offset, so many entries may name one: each
            // string is pushed once, keyed by where it is in the input (O(1) per entry, and the
            // arena no bigger than the index's strings).
            let mut pushed: HashMap<(usize, usize), Name> =
                HashMap::with_capacity_and_hasher(entries, Default::default());
            for symbol in symbols {
                // alloc: error path
                let symbol = symbol.map_err(|e| format!("the archive's index: {e}"))?;
                let name = std::str::from_utf8(symbol.name())
                    .map_err(|_| "a name in the archive's index that is not UTF-8")?;
                let member = file.member(symbol.offset()).map_err(|e| {
                    // alloc: error path
                    format!("the archive's index names `{name}` in no member: {e}")
                })?;
                let member = by_offset
                    .binary_search(&member.file_range().0)
                    // alloc: error path
                    .map_err(|_| format!("the archive's index names `{name}` in no member"))?;
                let name = match pushed.entry((name.as_ptr() as usize, name.len())) {
                    Entry::Occupied(it) => *it.get(),
                    Entry::Vacant(it) => *it.insert(names.push(name)?),
                };
                index.push((name, member as u32));
            }
        }
        None if members.is_empty() || whole => {}
        None => {
            let missing = "an archive without an index of its symbols (run `llvm-ranlib` on it)";
            // alloc: error path
            return Err(missing.to_owned());
        }
    }
    Ok(Archive { members, index, names })
}

struct Imported<'a> {
    module: &'a str,
    field: &'a str,
    ty: u32,
}

struct Custom<'a> {
    /// Its index among the module's sections, which relocations and symbols name it by.
    index: usize,
    name: &'a str,
    /// Its contents after the name, and where they start in the file.
    payload: Range<usize>,
}

/// The sections of an object, read. Each column is reserved once, exactly, by its section's
/// count (the imports' three by the import count): a constant number of buffers per object.
#[derive(Default)]
struct Parsed<'a> {
    sections: Vec<reader::Section<'a>>,
    /// Each type as the type section encodes it, checked: borrowed, never decoded.
    types: Vec<Sig<'a>>,
    imported: Vec<Imported<'a>>,
    imported_globals: Vec<&'a str>,
    imported_tables: Vec<&'a str>,
    /// The type of each function with a body.
    functions: Vec<u32>,
    /// Each function export: the function's index and the name.
    exports: Vec<(u32, &'a str)>,
    code: Option<usize>,
    /// Where each body is in the file, after its size.
    bodies: Vec<Range<usize>>,
    data: Option<usize>,
    /// Where each segment's bytes are in the file.
    segments: Vec<Range<usize>>,
    customs: Vec<Custom<'a>>,
    linking: Option<&'a [u8]>,
    relocs: Vec<(&'a str, &'a [u8])>,
}

fn parse(bytes: &[u8]) -> Result<Parsed<'_>, String> {
    let sections = reader::sections(bytes)?;
    let mut parsed = Parsed::default();
    let customs = sections.iter().filter(|it| it.id == id::CUSTOM).count();
    parsed.customs.reserve_exact(customs);
    parsed.relocs.reserve_exact(customs);
    for (index, section) in sections.iter().enumerate() {
        let mut r = Reader::new(section.contents);
        let at = section.at;
        // alloc: error path
        let what = |e: String| format!("section {index} (id {}): {e}", section.id);
        let mut one = || -> Result<(), String> {
            match section.id {
                id::CUSTOM => {
                    let name = r.name()?;
                    let payload = at + r.at()..section.range().end;
                    let contents = r.take_rest();
                    if name == "linking" {
                        parsed.linking = Some(contents);
                    } else if name.starts_with("reloc.") {
                        parsed.relocs.push((name, contents));
                    } else {
                        parsed.customs.push(Custom { index, name, payload });
                    }
                }
                id::TYPE => {
                    let count = r.count()?;
                    parsed.types.reserve_exact(count);
                    for _ in 0..count {
                        parsed.types.push(signature(&mut r, section.contents)?);
                    }
                }
                id::IMPORT => {
                    let count = r.count()?;
                    parsed.imported.reserve_exact(count);
                    parsed.imported_tables.reserve_exact(count);
                    parsed.imported_globals.reserve_exact(count);
                    for _ in 0..count {
                        let module = r.name()?;
                        let field = r.name()?;
                        match r.u8()? {
                            0 => parsed.imported.push(Imported { module, field, ty: r.uleb32()? }),
                            1 => {
                                r.u8()?;
                                limits(&mut r)?;
                                parsed.imported_tables.push(field);
                            }
                            2 => limits(&mut r)?,
                            3 => {
                                r.u8()?;
                                r.u8()?;
                                parsed.imported_globals.push(field);
                            }
                            4 => {
                                // alloc: error path
                                return Err(format!(
                                    "the tag import {module}.{field}: wasm exception handling \
                                     is not linked"
                                ));
                            }
                            // alloc: error path
                            kind => return Err(format!("the import {module}.{field} of kind {kind}")),
                        }
                    }
                }
                id::FUNCTION => {
                    let count = r.count()?;
                    parsed.functions.reserve_exact(count);
                    for _ in 0..count {
                        parsed.functions.push(r.uleb32()?);
                    }
                }
                id::EXPORT => {
                    let count = r.count()?;
                    parsed.exports.reserve_exact(count);
                    for _ in 0..count {
                        let name = r.name()?;
                        match (r.u8()?, r.uleb32()?) {
                            (0, function) => {
                                parsed.exports.push((function, name));
                            }
                            (kind, _) => {
                                // alloc: error path
                                return Err(format!(
                                    "the export `{name}` of kind {kind}: an object exports only \
                                     functions"
                                ));
                            }
                        }
                    }
                }
                id::CODE => {
                    parsed.code = Some(index);
                    let count = r.count()?;
                    parsed.bodies.reserve_exact(count);
                    for _ in 0..count {
                        let len = r.uleb32()? as usize;
                        let start = at + r.at();
                        r.take(len)?;
                        parsed.bodies.push(start..start + len);
                    }
                }
                id::DATA => {
                    parsed.data = Some(index);
                    let count = r.count()?;
                    parsed.segments.reserve_exact(count);
                    for _ in 0..count {
                        match r.uleb32()? {
                            0 => {}
                            1 => {
                                // alloc: error path
                                return Err("a passive data segment (shared memory)".to_owned());
                            }
                            2 => match r.uleb32()? {
                                0 => {}
                                memory => {
                                    // alloc: error path
                                    return Err(format!("a data segment of memory {memory}"));
                                }
                            },
                            // alloc: error path
                            flags => return Err(format!("a data segment of flags {flags}")),
                        }
                        r.i32_const()?;
                        let len = r.uleb32()? as usize;
                        let start = at + r.at();
                        r.take(len)?;
                        parsed.segments.push(start..start + len);
                    }
                }
                id::DATA_COUNT => {
                    r.uleb32()?;
                }
                id::TABLE | id::MEMORY | id::GLOBAL | id::START | id::ELEMENT | id::TAG => {
                    // alloc: error path
                    return Err(format!(
                        "a {} section, which LLVM's objects don't have: linked module or \
                         position-independent code?",
                        match section.id {
                            id::TABLE => "table",
                            id::MEMORY => "memory",
                            id::GLOBAL => "global",
                            id::START => "start",
                            id::ELEMENT => "element",
                            _ => "tag",
                        }
                    ));
                }
                // alloc: error path
                other => return Err(format!("a section of the unknown id {other}")),
            }
            if !r.is_done() {
                // alloc: error path
                return Err("bytes after its contents".to_owned());
            }
            Ok(())
        };
        one().map_err(what)?;
    }
    if parsed.bodies.len() != parsed.functions.len() {
        // alloc: error path
        return Err(format!(
            "{} function types and {} bodies",
            parsed.functions.len(),
            parsed.bodies.len()
        ));
    }
    parsed.sections = sections;
    Ok(parsed)
}

/// One function type, checked, as the type section has it.
fn signature<'a>(r: &mut Reader<'a>, contents: &'a [u8]) -> Result<Sig<'a>, String> {
    match contents.get(r.at()) {
        Some(0x60) => code::signature(r, contents),
        // alloc: error path
        Some(form) => Err(format!("a type of form {form:#04x} (GC types are not linked)")),
        // alloc: error path
        None => Err("a truncated type".to_owned()),
    }
}

fn limits(r: &mut Reader<'_>) -> Result<(), String> {
    let flags = r.u8()?;
    if flags > 7 {
        // alloc: error path
        return Err(format!("limits of flags {flags:#x}"));
    }
    r.uleb()?;
    if flags & 1 != 0 {
        r.uleb()?;
    }
    Ok(())
}

struct WasmSymbol<'a> {
    kind: u8,
    flags: u32,
    /// A function's, global's, table's or section's index.
    index: u32,
    name: Option<&'a str>,
    /// A defined data symbol's segment, offset and size.
    data: Option<(u32, u64, u64)>,
}

struct SegmentInfo<'a> {
    name: &'a str,
    align: u32,
    flags: u32,
}

/// Each column reserved once by its subsection's count (the COMDATs' entries by a bound from
/// the subsection's size): a constant number of buffers per object.
#[derive(Default)]
struct Linking<'a> {
    symbols: Vec<WasmSymbol<'a>>,
    segments: Vec<SegmentInfo<'a>>,
    inits: Vec<(u32, u32)>,
    /// Each COMDAT's name and its run of [`Linking::comdat_entries`].
    comdats: Vec<(&'a str, Range<u32>)>,
    /// Every COMDAT's entries, the kind and the index.
    comdat_entries: Vec<(u8, u32)>,
}

fn linking<'a>(contents: &'a [u8]) -> Result<Linking<'a>, String> {
    let mut r = Reader::new(contents);
    match r.uleb32()? {
        LINKING_VERSION => {}
        // alloc: error path
        version => return Err(format!("a linking section of version {version}")),
    }
    let mut linking = Linking::default();
    while !r.is_done() {
        let id = r.u8()?;
        let len = r.uleb32()? as usize;
        let mut s = Reader::new(r.take(len)?);
        match id {
            SEGMENT_INFO => {
                let count = s.count()?;
                linking.segments.reserve_exact(count);
                for _ in 0..count {
                    let name = s.name()?;
                    let align = s.uleb32()?;
                    let flags = s.uleb32()?;
                    linking.segments.push(SegmentInfo { name, align, flags });
                }
            }
            INIT_FUNCS => {
                let count = s.count()?;
                linking.inits.reserve_exact(count);
                for _ in 0..count {
                    let priority = s.uleb32()?;
                    linking.inits.push((priority, s.uleb32()?));
                }
            }
            COMDAT_INFO => {
                let count = s.count()?;
                linking.comdats.reserve_exact(count);
                // An entry is at least two bytes (its kind and a LEB128 index).
                linking.comdat_entries.reserve(len / 2);
                for _ in 0..count {
                    let name = s.name()?;
                    match s.uleb32()? {
                        0 => {}
                        // alloc: error path
                        flags => return Err(format!("the COMDAT `{name}` of flags {flags}")),
                    }
                    let start = linking.comdat_entries.len() as u32;
                    for _ in 0..s.count()? {
                        let kind = s.u8()?;
                        linking.comdat_entries.push((kind, s.uleb32()?));
                    }
                    linking.comdats.push((name, start..linking.comdat_entries.len() as u32));
                }
            }
            SYMBOL_TABLE => {
                let count = s.count()?;
                linking.symbols.reserve_exact(count);
                for _ in 0..count {
                    linking.symbols.push(symbol(&mut s)?);
                }
            }
            // alloc: error path
            other => return Err(format!("the linking subsection {other}")),
        }
        if !s.is_done() {
            // alloc: error path
            return Err(format!("bytes after the linking subsection {id}"));
        }
    }
    Ok(linking)
}

fn symbol<'a>(r: &mut Reader<'a>) -> Result<WasmSymbol<'a>, String> {
    let kind = r.u8()?;
    let flags = r.uleb32()?;
    let mut symbol = WasmSymbol { kind, flags, index: 0, name: None, data: None };
    match kind {
        SYM_FUNCTION | SYM_GLOBAL | SYM_TAG | SYM_TABLE => {
            symbol.index = r.uleb32()?;
            if flags & UNDEFINED == 0 || flags & EXPLICIT_NAME != 0 {
                symbol.name = Some(r.name()?);
            }
        }
        SYM_DATA => {
            symbol.name = Some(r.name()?);
            if flags & UNDEFINED == 0 {
                let segment = r.uleb32()?;
                let offset = r.uleb()?;
                symbol.data = Some((segment, offset, r.uleb()?));
            }
        }
        SYM_SECTION => symbol.index = r.uleb32()?,
        // alloc: error path
        other => return Err(format!("a symbol of the unknown kind {other}")),
    }
    Ok(symbol)
}

/// 24 bytes.
struct Reloc {
    r_type: u32,
    /// From the start of the section's contents (after its id and size).
    offset: usize,
    index: u32,
    addend: i64,
}

const _: () = assert!(size_of::<Reloc>() <= 24);

/// A function's names: the first a symbol gives it, the first an exported symbol gives it,
/// and the last the export section gives it.
#[derive(Clone, Copy, Default)]
struct FunctionNames<'a> {
    first: Option<&'a str>,
    exported: Option<&'a str>,
    export: Option<&'a str>,
}

/// A relocation section's header: the section it is of and how many relocations it holds.
fn relocs_header(r: &mut Reader<'_>) -> Result<(usize, usize), String> {
    Ok((r.uleb32()? as usize, r.count()?))
}

/// Appends a relocation section's relocations to `out` (which has room for them) and says
/// which section they are of.
fn relocs(contents: &[u8], out: &mut Vec<Reloc>) -> Result<usize, String> {
    let mut r = Reader::new(contents);
    let (section, count) = relocs_header(&mut r)?;
    for _ in 0..count {
        let r_type = r.uleb32()?;
        let offset = r.uleb32()? as usize;
        let index = r.uleb32()?;
        let addend = if reloc::has_addend(r_type) { r.sleb()? } else { 0 };
        out.push(Reloc { r_type, offset, index, addend });
    }
    if !r.is_done() {
        // alloc: error path
        return Err("bytes after the relocations".to_owned());
    }
    Ok(section)
}

/// What a data segment's name places it in, as wasm-ld names output segments.
fn segment_class(name: &str, flags: u32) -> Result<Class, String> {
    let is = |prefix: &str| {
        name.strip_prefix(prefix).is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
    };
    if flags & SEGMENT_TLS != 0 || is(".tdata") || is(".tbss") {
        // alloc: error path
        return Err(format!(
            "the thread-local data segment `{name}`: wasm32 without shared memory has no TLS \
             (LLVM makes thread-locals plain data there)"
        ));
    }
    Ok(if is(".rodata") {
        Class::Rodata
    } else if is(".data") {
        Class::Data
    } else if is(".bss") {
        Class::Bss
    } else {
        super::named_data(name)
    })
}

/// The bytes a relocation of `width` bytes at `at` in the file falls in: the index of the
/// range among `ranges` (sorted, disjoint) and the offset in it.
fn within(ranges: &[Range<usize>], at: usize, width: usize) -> Option<(usize, usize)> {
    let after = ranges.partition_point(|it| it.start <= at);
    let index = after.checked_sub(1)?;
    let range = &ranges[index];
    (at + width <= range.end).then_some((index, at - range.start))
}

fn width(r_type: u32) -> usize {
    match r_type {
        reloc::TABLE_INDEX_I32
        | reloc::MEMORY_ADDR_I32
        | reloc::GLOBAL_INDEX_I32
        | reloc::FUNCTION_INDEX_I32
        | reloc::FUNCTION_OFFSET_I32
        | reloc::SECTION_OFFSET_I32 => 4,
        _ => super::leb::PADDED,
    }
}

fn object(bytes: &[u8]) -> Result<Object, String> {
    let parsed = parse(bytes)?;
    let linking = linking(parsed.linking.ok_or(
        "no linking section: not a relocatable object (a linked module can't be linked again)",
    )?)?;
    if linking.segments.len() != parsed.segments.len() {
        // alloc: error path
        return Err(format!(
            "{} data segments and {} of them in the linking section",
            parsed.segments.len(),
            linking.segments.len()
        ));
    }
    let imported = parsed.imported.len() as u32;

    // Every relocation in one column, each section's a run of it, by the section's index in
    // the module: the headers are read first so the column is one exact allocation.
    let mut total = 0;
    for &(name, contents) in &parsed.relocs {
        // alloc: error path
        let (_, count) = relocs_header(&mut Reader::new(contents)).map_err(|e| format!("{name}: {e}"))?;
        total += count;
    }
    let mut all_relocs: Vec<Reloc> = Vec::with_capacity(total);
    // alloc: once per object, a run per section
    let mut runs: Vec<Range<u32>> = vec![0..0; parsed.sections.len()];
    for &(name, contents) in &parsed.relocs {
        let start = all_relocs.len() as u32;
        // alloc: error path
        let section = relocs(contents, &mut all_relocs).map_err(|e| format!("{name}: {e}"))?;
        let Some(run) = runs.get_mut(section) else {
            // alloc: error path
            return Err(format!("{name} is of section {section}, which there is not"));
        };
        if !run.is_empty() {
            // alloc: error path
            return Err(format!("{name}: a second relocation section of section {section}"));
        }
        *run = start..all_relocs.len() as u32;
    }
    let relocs_of = |section: usize| &all_relocs[runs[section].start as usize..runs[section].end as usize];
    let start_of = |section: usize| parsed.sections[section].at;
    let named = |r: &Reloc| reloc::name(r.r_type).unwrap_or("an unknown relocation");

    let bodies = parsed.bodies.len();
    let mut sections =
        Vec::with_capacity(bodies + parsed.segments.len() + parsed.customs.len() + linking.inits.len());
    // Every section's relocations, each section's a run: the core's one column.
    let mut fixups: Vec<NativeFixup> = Vec::with_capacity(all_relocs.len() + linking.inits.len());
    // Every name once, borrowed from the input and pushed: the functions' and init arrays'
    // section names are built in `scratch`, one buffer for all of them, room for the longest
    // (`.text.` and a name or an index, `.init_array.` and a priority).
    let symbol_names: usize = linking.symbols.iter().filter_map(|it| it.name).map(str::len).sum();
    let longest = linking.symbols.iter().filter_map(|it| it.name).map(str::len).max().unwrap_or(0);
    let segment_names: usize = linking.segments.iter().map(|it| it.name.len()).sum();
    let custom_names: usize = parsed.customs.iter().map(|it| it.name.len()).sum();
    let comdat_names: usize = linking.comdats.iter().map(|it| it.0.len()).sum();
    // An imported function's module and field once for its `WasmImport`, the field again as
    // its symbol's name when the symbol has none.
    let fields: usize = parsed.imported.iter().map(|it| it.module.len() + 2 * it.field.len()).sum::<usize>()
        + parsed.imported_globals.iter().chain(&parsed.imported_tables).map(|it| it.len()).sum::<usize>();
    // A symbol's name once for it and once more in its function's section name, `.text.` and
    // an index (at most 16 bytes) or `.init_array.` and a priority (at most 17).
    let mut names = Names::with_capacity(
        symbol_names * 2
            + fields
            + segment_names
            + custom_names
            + comdat_names
            + 24 * (bodies + linking.inits.len()),
    );
    let mut scratch = String::with_capacity(longest + 24);
    let undefined_functions =
        linking.symbols.iter().filter(|it| it.kind == SYM_FUNCTION && it.flags & UNDEFINED != 0).count();
    let mut imports = Vec::with_capacity(undefined_functions);

    // alloc: once per object, an entry per function
    let mut function_names = vec![FunctionNames::default(); bodies];
    for symbol in &linking.symbols {
        if symbol.kind != SYM_FUNCTION || symbol.flags & UNDEFINED != 0 {
            continue;
        }
        let (Some(name), Some(body)) = (symbol.name, symbol.index.checked_sub(imported)) else {
            continue;
        };
        if let Some(slot) = function_names.get_mut(body as usize) {
            slot.first.get_or_insert(name);
            if symbol.flags & EXPORTED != 0 {
                slot.exported.get_or_insert(name);
            }
        }
    }
    // The code's relocations as one list sorted by body (CSR, 16 bytes each): the body, the
    // offset in it, the relocation. LLVM lists them in offset order, which the adaptive sort
    // takes in one pass.
    let code_relocs_of = parsed.code.map_or(&[][..], relocs_of);
    let mut code_relocs: Vec<(u32, u32, &Reloc)> = Vec::with_capacity(code_relocs_of.len());
    if let Some(code) = parsed.code {
        for r in code_relocs_of {
            let at = start_of(code) + r.offset;
            let (body, offset) = within(&parsed.bodies, at, width(r.r_type)).ok_or_else(|| {
                // alloc: error path
                format!("{} at {:#x} of the code section, in no function body", named(r), r.offset)
            })?;
            code_relocs.push((body as u32, offset as u32, r));
        }
        radix::sort_by_key(&mut code_relocs, |it| radix::pack_32(it.0, it.1));
    }
    // The last export of each body wins, as the export section's order says.
    for &(function, name) in &parsed.exports {
        if let Some(slot) = function.checked_sub(imported).and_then(|it| function_names.get_mut(it as usize)) {
            slot.export = Some(name);
        }
    }
    let mut function_sections = Vec::with_capacity(bodies);
    // Reused by every function, room for the most any can need: the types its
    // `call_indirect`s name and where.
    let mut types: Vec<Sig<'_>> = Vec::with_capacity(parsed.types.len());
    let mut type_fixups: Vec<(u32, u32)> = Vec::with_capacity(code_relocs.len());
    // Every function's code, header then body, one after the other in one buffer. A header is
    // at most 23 bytes and its type, then 10 bytes and a type per `call_indirect`, and its
    // export's name (a symbol's or an export's).
    let longest_type = parsed.types.iter().map(|it| it.len()).max().unwrap_or(0);
    let body_bytes: usize = parsed.bodies.iter().map(|it| it.len()).sum();
    let export_names: usize = parsed.exports.iter().map(|it| it.1.len()).sum();
    let mut made = Vec::with_capacity(
        body_bytes
            + bodies * (23 + longest_type)
            + code_relocs.len() * (10 + longest_type)
            + symbol_names
            + export_names,
    );
    let mut next = 0;
    for (body, range) in parsed.bodies.iter().enumerate() {
        let index = imported + body as u32;
        let ty = parsed.functions[body];
        // alloc: error path
        let ty = *parsed.types.get(ty as usize).ok_or_else(|| format!("no type {ty}"))?;
        let end = next + code_relocs[next..].partition_point(|it| it.0 as usize == body);
        let relocs = &code_relocs[next..end];
        next = end;
        types.clear();
        type_fixups.clear();
        let run = fixups.len();
        for &(_, at, r) in relocs {
            match reloc::kind(r.r_type)? {
                Kind::Applied => fixups.push(NativeFixup {
                    at: at.into(),
                    apply: Apply::Native(r.r_type),
                    symbol: r.index,
                    addend: r.addend,
                    minus: None,
                }),
                Kind::Type => {
                    let ty = *parsed
                        .types
                        .get(r.index as usize)
                        // alloc: error path
                        .ok_or_else(|| format!("{}: no type {}", named(r), r.index))?;
                    // A function names a few types: a scan beats a map.
                    let which = match types.iter().position(|&it| it == ty) {
                        Some(which) => which,
                        None => {
                            types.push(ty);
                            types.len() - 1
                        }
                    };
                    type_fixups.push((at, which as u32));
                }
                Kind::Debug => {
                    // alloc: error path
                    return Err(format!("{} in the code of function {index}", named(r)));
                }
            }
        }
        let FunctionNames { first, exported, export } = function_names[body];
        let export = export.or(exported);
        let parts = Parts { ty, types: &types, type_fixups: &type_fixups, export };
        let from = made.len() as u64;
        let start = parts.encode_into(&mut made, &bytes[range.start..range.end]);
        for fixup in &mut fixups[run..] {
            fixup.at += u64::from(start);
        }
        scratch.clear();
        scratch.push_str(".text.");
        match first {
            Some(it) => scratch.push_str(it),
            None => {
                use std::fmt::Write;
                let _ = write!(scratch, "{index}");
            }
        }
        function_sections.push(sections.len() as u32);
        sections.push(Section {
            name: names.push(&scratch)?,
            what: SectionKind::Alloc(Class::Text, 0),
            align: 1,
            // The header the reader puts before the body: not the input's bytes as they are.
            data: Data::Made(from..made.len() as u64),
            fixups: run as u32..fixups.len() as u32,
            retain: export.is_some(),
        });
    }

    // Data segments. Their relocations, by segment: LLVM lists them in offset order, so by
    // segment already, and the adaptive sort takes them in one pass.
    let data_relocs = parsed.data.map_or(&[][..], relocs_of);
    let mut segment_fixups: Vec<(u32, NativeFixup)> = Vec::with_capacity(data_relocs.len());
    if let Some(data) = parsed.data {
        for r in data_relocs {
            let at = start_of(data) + r.offset;
            let (segment, offset) = within(&parsed.segments, at, width(r.r_type)).ok_or_else(|| {
                // alloc: error path
                format!("{} at {:#x} of the data section, in no segment", named(r), r.offset)
            })?;
            match reloc::kind(r.r_type)? {
                Kind::Applied => segment_fixups.push((
                    segment as u32,
                    NativeFixup {
                        at: offset as u64,
                        apply: Apply::Native(r.r_type),
                        symbol: r.index,
                        addend: r.addend,
                        minus: None,
                    },
                )),
                Kind::Type | Kind::Debug => {
                    // alloc: error path
                    return Err(format!("{} in the data segment {segment}", named(r)));
                }
            }
        }
    }
    radix::sort_copy_by_key(&mut segment_fixups, |it| it.0);
    let mut next = 0;
    let mut segment_sections = Vec::with_capacity(parsed.segments.len());
    for (segment, (range, info)) in parsed.segments.iter().zip(&linking.segments).enumerate() {
        let class = segment_class(info.name, info.flags)?;
        let contents = &bytes[range.start..range.end];
        let data = match class {
            Class::Bss if contents.iter().all(|&it| it == 0) => Data::Zero(contents.len() as u64),
            Class::Bss => {
                // alloc: error path
                return Err(format!("the zeroed segment `{}` holds bytes other than 0", info.name));
            }
            _ => Data::Input(range.start as u64..range.end as u64),
        };
        if info.align >= 32 {
            // alloc: error path
            return Err(format!("the segment `{}` aligned to 2^{}", info.name, info.align));
        }
        let end = next + segment_fixups[next..].partition_point(|it| it.0 as usize == segment);
        let run = fixups.len() as u32;
        fixups.extend(segment_fixups[next..end].iter().map(|it| it.1));
        next = end;
        segment_sections.push(sections.len() as u32);
        sections.push(Section {
            name: names.push(info.name)?,
            what: SectionKind::Alloc(class, 0),
            align: 1 << info.align,
            data,
            fixups: run..fixups.len() as u32,
            retain: info.flags & SEGMENT_RETAIN != 0,
        });
    }

    // Custom sections.
    // The contract section of each custom section, by the custom section's index.
    // alloc: once per object, an entry per section
    let mut custom_sections: Vec<Option<u32>> = vec![None; parsed.sections.len()];
    for custom in &parsed.customs {
        let dropped = custom.name.starts_with(".debug")
            || matches!(custom.name, "name" | "external_debug_info" | "sourceMappingURL");
        if custom.name.starts_with("dylink") {
            // alloc: error path
            return Err(format!(
                "the `{}` section of a position-independent object, which a wasm32 module \
                 doesn't link",
                custom.name
            ));
        }
        let run = fixups.len() as u32;
        for r in relocs_of(custom.index) {
            let at = start_of(custom.index) + r.offset;
            let offset = at
                .checked_sub(custom.payload.start)
                .filter(|&it| it + width(r.r_type) <= custom.payload.len())
                // alloc: error path
                .ok_or_else(|| format!("{} outside the section `{}`", named(r), custom.name))?;
            match (reloc::kind(r.r_type)?, dropped) {
                (Kind::Applied, _) | (Kind::Debug, true) => fixups.push(NativeFixup {
                    at: offset as u64,
                    apply: Apply::Native(r.r_type),
                    symbol: r.index,
                    addend: r.addend,
                    minus: None,
                }),
                (Kind::Type | Kind::Debug, _) => {
                    // alloc: error path
                    return Err(format!("{} in the section `{}`", named(r), custom.name));
                }
            }
        }
        custom_sections[custom.index] = Some(sections.len() as u32);
        sections.push(Section {
            name: names.push(custom.name)?,
            what: match dropped {
                true => SectionKind::Dropped,
                false => SectionKind::Alloc(super::custom(custom.name), 0),
            },
            align: 1,
            data: Data::Input(custom.payload.start as u64..custom.payload.end as u64),
            fixups: run..fixups.len() as u32,
            retain: !dropped,
        });
    }
    let function_section = |index: u32| {
        index
            .checked_sub(imported)
            .and_then(|body| function_sections.get(body as usize).copied())
            // alloc: error path
            .ok_or_else(|| format!("function {index}, which the object doesn't define"))
    };

    // Symbols.
    let mut symbols = Vec::with_capacity(linking.symbols.len());
    for (at, s) in linking.symbols.iter().enumerate() {
        let binding = if s.flags & BINDING_LOCAL != 0 {
            Binding::Local
        } else if s.flags & BINDING_WEAK != 0 {
            Binding::Weak
        } else {
            Binding::Global
        };
        let hidden = s.flags & VISIBILITY_HIDDEN != 0;
        let undefined = s.flags & UNDEFINED != 0;
        // alloc: error path
        let unnamed = || format!("symbol {at} has no name");
        let symbol = match s.kind {
            SYM_FUNCTION if undefined => {
                let import = parsed
                    .imported
                    .get(s.index as usize)
                    // alloc: error path
                    .ok_or_else(|| format!("symbol {at}: no imported function {}", s.index))?;
                let ty = parsed
                    .types
                    .get(import.ty as usize)
                    // alloc: error path
                    .ok_or_else(|| format!("symbol {at}: no type {}", import.ty))?;
                // The type section's types are slices of the input, checked when it was read.
                let ty_at = (ty.as_ptr() as usize - bytes.as_ptr() as usize) as u64;
                imports.push((
                    at as u32,
                    WasmImport {
                        module: names.push(import.module)?,
                        field: names.push(import.field)?,
                        ty: ty_at..ty_at + ty.len() as u64,
                    },
                ));
                Symbol {
                    name: names.push(s.name.unwrap_or(import.field))?,
                    def: SymbolDef::Undefined,
                    binding,
                    kind: SymbolKind::Function,
                    hidden,
                }
            }
            SYM_FUNCTION => {
                // alloc: error path
                let section = function_section(s.index).map_err(|e| format!("symbol {at}: {e}"))?;
                if s.flags & (EXPORTED | NO_STRIP) != 0 {
                    sections[section as usize].retain = true;
                }
                Symbol {
                    name: names.push(s.name.ok_or_else(unnamed)?)?,
                    def: SymbolDef::Section {
                        section,
                        value: 0,
                        size: sections[section as usize].data.len(),
                    },
                    binding,
                    kind: SymbolKind::Function,
                    hidden,
                }
            }
            SYM_DATA => {
                let name = s.name.ok_or_else(unnamed)?;
                if s.flags & TLS != 0 {
                    // alloc: error path
                    return Err(format!(
                        "the thread-local `{name}`: wasm32 without shared memory has no TLS"
                    ));
                }
                if s.flags & EXPORTED != 0 {
                    // alloc: error path
                    return Err(format!("`{name}` is data and exported: only functions are"));
                }
                let def = match s.data {
                    None => SymbolDef::Undefined,
                    Some((_, offset, _)) if s.flags & ABSOLUTE != 0 => SymbolDef::Absolute(offset),
                    Some((segment, value, size)) => {
                        let section = *segment_sections
                            .get(segment as usize)
                            // alloc: error path
                            .ok_or_else(|| format!("`{name}` is in no segment {segment}"))?;
                        let len = sections[section as usize].data.len();
                        if value.checked_add(size).is_none_or(|end| end > len) {
                            // alloc: error path
                            return Err(format!("`{name}` reaches past its segment"));
                        }
                        if s.flags & NO_STRIP != 0 {
                            sections[section as usize].retain = true;
                        }
                        SymbolDef::Section { section, value, size }
                    }
                };
                let kind = SymbolKind::Data;
                Symbol { name: names.push(name)?, def, binding, kind, hidden }
            }
            SYM_GLOBAL | SYM_TABLE if undefined => {
                let fields = match s.kind {
                    SYM_GLOBAL => &parsed.imported_globals,
                    _ => &parsed.imported_tables,
                };
                let name = s.name.or_else(|| fields.get(s.index as usize).copied());
                Symbol {
                    name: names.push(name.ok_or_else(unnamed)?)?,
                    def: SymbolDef::Undefined,
                    binding,
                    kind: SymbolKind::Other,
                    hidden,
                }
            }
            SYM_GLOBAL | SYM_TABLE => {
                // alloc: error path
                return Err(format!(
                    "symbol {at} defines a {}: the module's globals and table are the linker's",
                    if s.kind == SYM_GLOBAL { "global" } else { "table" }
                ));
            }
            SYM_SECTION => {
                let section = custom_sections
                    .get(s.index as usize)
                    .copied()
                    .flatten()
                    // alloc: error path
                    .ok_or_else(|| format!("symbol {at} of section {}, no custom one", s.index))?;
                Symbol {
                    name: sections[section as usize].name,
                    def: SymbolDef::Section { section, value: 0, size: 0 },
                    binding: Binding::Local,
                    kind: SymbolKind::Section,
                    hidden: false,
                }
            }
            SYM_TAG => {
                // alloc: error path
                return Err(format!(
                    "symbol {at} is an exception tag: wasm exception handling is not linked"
                ));
            }
            // alloc: error path
            other => return Err(format!("symbol {at} of the unknown kind {other}")),
        };
        symbols.push(symbol);
    }

    // Init functions: each a pointer in the init array, called by `__wasm_call_ctors`.
    for &(priority, symbol) in &linking.inits {
        match linking.symbols.get(symbol as usize) {
            Some(it) if it.kind == SYM_FUNCTION => {}
            // alloc: error path
            _ => return Err(format!("the init function {symbol} is no function's symbol")),
        }
        scratch.clear();
        {
            use std::fmt::Write;
            let _ = write!(scratch, ".init_array.{priority}");
        }
        let run = fixups.len() as u32;
        fixups.push(NativeFixup {
            at: 0,
            apply: Apply::Native(reloc::TABLE_INDEX_I32),
            symbol,
            addend: 0,
            minus: None,
        });
        sections.push(Section {
            name: names.push(&scratch)?,
            what: SectionKind::Alloc(Class::InitArray, priority),
            align: 4,
            // The slot the core fills (it writes a zeroed piece's fixups too): no bytes of the
            // input, and none made.
            data: Data::Zero(4),
            fixups: run..fixups.len() as u32,
            retain: false,
        });
    }

    let mut groups = Vec::with_capacity(linking.comdats.len());
    let mut grouped = Vec::with_capacity(linking.comdat_entries.len());
    for (name, entries) in &linking.comdats {
        let start = grouped.len() as u32;
        for &(kind, index) in &linking.comdat_entries[entries.start as usize..entries.end as usize] {
            grouped.push(match kind {
                COMDAT_DATA => *segment_sections
                    .get(index as usize)
                    // alloc: error path
                    .ok_or_else(|| format!("the COMDAT `{name}` has no segment {index}"))?,
                COMDAT_FUNCTION => {
                    // alloc: error path
                    function_section(index).map_err(|e| format!("the COMDAT `{name}`: {e}"))?
                }
                COMDAT_SECTION => custom_sections
                    .get(index as usize)
                    .copied()
                    .flatten()
                    // alloc: error path
                    .ok_or_else(|| format!("the COMDAT `{name}` has no custom section {index}"))?,
                // alloc: error path
                other => return Err(format!("the COMDAT `{name}` holds a thing of kind {other}")),
            });
        }
        groups.push(Group { name: names.push(name)?, sections: start..grouped.len() as u32, largest: false });
    }

    for fixup in &fixups {
        if fixup.symbol as usize >= symbols.len() {
            // alloc: error path
            return Err(format!("a relocation against symbol {}, which there is not", fixup.symbol));
        }
    }
    Ok(Object {
        sections,
        symbols,
        fixups,
        groups,
        grouped,
        // A wasm object has no ties, live support, libraries or directives.
        // alloc: empty, allocates nothing
        ties: Vec::new(),
        // alloc: empty, allocates nothing
        live_support: Vec::new(),
        names,
        imports,
        made,
        // alloc: empty, allocates nothing
        libraries: Vec::new(),
        // alloc: empty, allocates nothing
        directives: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use blaze_codegen::wasm::{Code, Signature, ValType};

    use super::*;
    use crate::wasm::leb::{custom, name, section, sleb, uleb};

    const SP: u32 = 3;

    fn vec_of(count: usize, entries: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        uleb(&mut out, count as u64);
        out.extend(entries);
        out
    }

    fn subsection(out: &mut Vec<u8>, id: u8, contents: &[u8]) {
        out.push(id);
        uleb(out, contents.len() as u64);
        out.extend(contents);
    }

    /// The body of `_start`: the places of its relocations, from the start of the body.
    const START_BODY: [u8; 38] = [
        0x00, // no locals
        0x23, 0x80, 0x80, 0x80, 0x80, 0x00, // global.get __stack_pointer (2)
        0x1a, // drop
        0x41, 0x80, 0x80, 0x80, 0x80, 0x00, // i32.const msg+2 (9)
        0x41, 0x80, 0x80, 0x80, 0x80, 0x00, // i32.const slot of init (15)
        0x11, 0x80, 0x80, 0x80, 0x80, 0x00, // call_indirect type (21)
        0x80, 0x80, 0x80, 0x80, 0x00, // table number (26)
        0x10, 0x80, 0x80, 0x80, 0x80, 0x00, // call fd_write (32)
        0x0b,
    ];

    /// Relocations: type, offset in the section, symbol or type, addend.
    fn reloc_section(out: &mut Vec<u8>, name_: &str, section_index: u32, relocs: &[(u8, u32, u32, i64)]) {
        let mut contents = Vec::new();
        uleb(&mut contents, section_index.into());
        uleb(&mut contents, relocs.len() as u64);
        for &(r_type, offset, index, addend) in relocs {
            contents.push(r_type);
            uleb(&mut contents, offset.into());
            uleb(&mut contents, index.into());
            if reloc::has_addend(r_type.into()) {
                sleb(&mut contents, addend);
            }
        }
        custom(out, name_, &contents);
    }

    /// A wasm object as clang writes one, with every kind of thing the reader takes.
    fn object_bytes(code_relocs: &[(u8, u32, u32, i64)], extra_symbol: Option<&[u8]>) -> Vec<u8> {
        let mut out = reader::MAGIC.to_vec();
        // Types: 0 is (i32 i32 i32 i32) -> i32, 1 is () -> (), 2 is (i32) -> ().
        section(&mut out, 1, &vec_of(3, &[0x60, 4, 0x7f, 0x7f, 0x7f, 0x7f, 1, 0x7f, 0x60, 0, 0, 0x60, 1, 0x7f, 0]));
        let mut imports = Vec::new();
        name(&mut imports, "env");
        name(&mut imports, "__linear_memory");
        imports.extend([2, 0, 1]);
        name(&mut imports, "env");
        name(&mut imports, "__stack_pointer");
        imports.extend([3, 0x7f, 1]);
        name(&mut imports, "wasi_snapshot_preview1");
        name(&mut imports, "fd_write");
        imports.extend([0, 0]);
        name(&mut imports, "env");
        name(&mut imports, "__indirect_function_table");
        imports.extend([1, 0x70, 0, 0]);
        section(&mut out, 2, &vec_of(4, &imports));
        section(&mut out, 3, &vec_of(2, &[1, 1]));
        let mut exports = Vec::new();
        name(&mut exports, "_start");
        exports.extend([0, 1]);
        section(&mut out, 7, &vec_of(1, &exports));
        // Code: the count, then each body's size and the body; `_start` starts at 2.
        let mut code = vec![2, START_BODY.len() as u8];
        code.extend(START_BODY);
        code.extend([2, 0x00, 0x0b]);
        section(&mut out, 10, &code);
        // Data: four segments, whose bytes are at 6, 15, 28 and 37 of the section.
        let mut data = vec![4];
        data.extend([0, 0x41, 0, 0x0b, 4]);
        data.extend(b"hi!\0");
        data.extend([0, 0x41, 4, 0x0b, 8]);
        data.extend([0; 8]);
        data.extend([0, 0x41, 12, 0x0b, 4]);
        data.extend([0; 4]);
        data.extend([0, 0x41, 16, 0x0b, 4]);
        data.extend([9; 4]);
        section(&mut out, 11, &data);
        let mut producers = vec![1];
        name(&mut producers, "language");
        producers.push(1);
        name(&mut producers, "C");
        name(&mut producers, "");
        custom(&mut out, "producers", &producers);
        custom(&mut out, ".debug_info", &[1, 2, 3, 4]);

        let mut linking = Vec::new();
        uleb(&mut linking, LINKING_VERSION.into());
        let mut symbols = Vec::new();
        let mut count = 0;
        let mut sym = |bytes: &[u8]| {
            symbols.extend(bytes);
            count += 1;
        };
        let mut entry = vec![SYM_FUNCTION, EXPORTED as u8, 1];
        name(&mut entry, "_start");
        sym(&entry);
        let mut entry = vec![SYM_FUNCTION, BINDING_LOCAL as u8, 2];
        name(&mut entry, "init");
        sym(&entry);
        let mut entry = vec![SYM_FUNCTION, (UNDEFINED | EXPLICIT_NAME) as u8, 0];
        name(&mut entry, "__imported_wasi_snapshot_preview1_fd_write");
        sym(&entry);
        sym(&[SYM_GLOBAL, UNDEFINED as u8, 0]);
        let mut entry = vec![SYM_DATA, 0];
        name(&mut entry, "msg");
        entry.extend([0, 0, 4]);
        sym(&entry);
        let mut entry = vec![SYM_DATA, VISIBILITY_HIDDEN as u8];
        name(&mut entry, "buf");
        entry.extend([1, 0, 8]);
        sym(&entry);
        let mut entry = vec![SYM_DATA, 0];
        name(&mut entry, "ptr");
        entry.extend([2, 0, 4]);
        sym(&entry);
        let mut entry = vec![SYM_DATA, UNDEFINED as u8];
        name(&mut entry, "extern_data");
        sym(&entry);
        sym(&[SYM_TABLE, UNDEFINED as u8, 0]);
        sym(&[SYM_SECTION, BINDING_LOCAL as u8, 7]);
        let mut entry = vec![SYM_DATA, 0x80, 0x01];
        name(&mut entry, "items");
        entry.extend([3, 0, 4]);
        sym(&entry);
        if let Some(extra) = extra_symbol {
            sym(extra);
        }
        let mut segments = Vec::new();
        for (segment, align) in [(".rodata.msg", 0), (".bss.buf", 3), (".data.ptr", 2), ("my_items", 2)] {
            name(&mut segments, segment);
            segments.extend([align, 0]);
        }
        subsection(&mut linking, SEGMENT_INFO, &vec_of(4, &segments));
        subsection(&mut linking, INIT_FUNCS, &vec_of(1, &[101, 1]));
        let mut comdat = Vec::new();
        name(&mut comdat, "grp");
        comdat.extend([0, 2, COMDAT_FUNCTION, 2, COMDAT_DATA, 2]);
        subsection(&mut linking, COMDAT_INFO, &vec_of(1, &comdat));
        subsection(&mut linking, SYMBOL_TABLE, &vec_of(count, &symbols));
        custom(&mut out, "linking", &linking);

        reloc_section(&mut out, "reloc.CODE", 4, code_relocs);
        reloc_section(&mut out, "reloc.DATA", 5, &[(2, 28, 1, 0)]);
        // `.debug_info`'s contents start with its name: 12 bytes.
        reloc_section(&mut out, "reloc..debug_info", 7, &[(9, 12, 9, 0)]);
        out
    }

    fn code_relocs() -> Vec<(u8, u32, u32, i64)> {
        vec![(7, 4, SP, 0), (4, 11, 4, 2), (1, 17, 1, 0), (6, 23, 2, 0), (20, 28, 8, 0), (0, 34, 2, 0)]
    }

    fn read_object(bytes: &[u8]) -> Result<Object, String> {
        match read("crt1.o", bytes, &NativeKind::Object)? {
            NativeFile::Object(object) => Ok(object),
            _ => panic!("not an object"),
        }
    }

    #[test]
    fn reads_an_object() {
        let input = object_bytes(&code_relocs(), None);
        let object = read_object(&input).unwrap();
        let names: Vec<&str> = object.sections.iter().map(|it| object.name(it.name)).collect();
        assert_eq!(
            names,
            [
                ".text._start",
                ".text.init",
                ".rodata.msg",
                ".bss.buf",
                ".data.ptr",
                "my_items",
                "producers",
                ".debug_info",
                ".init_array.101",
            ]
        );
        let kinds: Vec<&SectionKind> = object.sections.iter().map(|it| &it.what).collect();
        assert_eq!(kinds[0], &SectionKind::Alloc(Class::Text, 0));
        assert_eq!(kinds[2], &SectionKind::Alloc(Class::Rodata, 0));
        assert_eq!(kinds[3], &SectionKind::Alloc(Class::Bss, 0));
        assert_eq!(kinds[4], &SectionKind::Alloc(Class::Data, 0));
        assert_eq!(kinds[5], &SectionKind::Alloc(crate::wasm::named_data("my_items"), 0));
        assert_eq!(kinds[6], &SectionKind::Alloc(crate::wasm::custom("producers"), 0));
        assert_eq!(kinds[7], &SectionKind::Dropped);
        assert_eq!(kinds[8], &SectionKind::Alloc(Class::InitArray, 101));
        assert_eq!(object.sections[8].data, Data::Zero(4));
        assert_eq!(object.sections[3].data, Data::Zero(8));
        assert_eq!(object.sections[3].align, 8);
        // Segments and custom sections are ranges of the input, not copies.
        assert!(matches!(object.sections[2].data, Data::Input(_)));
        assert_eq!(object.section_bytes(&object.sections[2].data, &input).unwrap(), Some(&b"hi!\0"[..]));
        assert!(matches!(object.sections[6].data, Data::Input(_)));
        assert_eq!(object.section_bytes(&object.sections[3].data, &input).unwrap(), None);
        let retained: Vec<bool> = object.sections.iter().map(|it| it.retain).collect();
        assert_eq!(retained, [true, false, false, false, false, true, true, false, false]);

        // `_start` is in the form codegen gives functions: header, then body.
        // Both functions' code is the reader's one buffer of made bytes, `_start`'s first.
        let Data::Made(made) = &object.sections[0].data else { panic!("not made") };
        let Data::Made(init) = &object.sections[1].data else { panic!("not made") };
        assert_eq!((made.start, made.end), (0, init.start));
        assert_eq!(init.end as usize, object.made.len());
        let start = object.section_bytes(&object.sections[0].data, &input).unwrap().unwrap();
        let (code, body) = Code::decode(start).unwrap();
        assert_eq!(&start[body..], &START_BODY);
        assert_eq!(code.ty, Signature::default());
        assert_eq!(code.types, [Signature { params: vec![ValType::I32], results: vec![] }]);
        assert_eq!(code.type_fixups, [(21, 0)]);
        assert_eq!(code.export.as_deref(), Some("_start"));
        let fixups: Vec<(u64, Apply, u32, i64)> = object
            .section_fixups(&object.sections[0])
            .iter()
            .map(|it| (it.at - body as u64, it.apply, it.symbol, it.addend))
            .collect();
        assert_eq!(
            fixups,
            [
                (2, Apply::Native(reloc::GLOBAL_INDEX_LEB), SP, 0),
                (9, Apply::Native(reloc::MEMORY_ADDR_SLEB), 4, 2),
                (15, Apply::Native(reloc::TABLE_INDEX_SLEB), 1, 0),
                (26, Apply::Native(reloc::TABLE_NUMBER_LEB), 8, 0),
                (32, Apply::Native(reloc::FUNCTION_INDEX_LEB), 2, 0),
            ]
        );
        assert_eq!(
            object.section_fixups(&object.sections[4]),
            [NativeFixup {
                at: 0,
                apply: Apply::Native(reloc::TABLE_INDEX_I32),
                symbol: 1,
                addend: 0,
                minus: None
            }]
        );
        assert_eq!(object.section_fixups(&object.sections[8])[0].symbol, 1);
        assert_eq!(
            object.section_fixups(&object.sections[7])[0].apply,
            Apply::Native(reloc::SECTION_OFFSET_I32)
        );
        // Each section's run of the one column, in section order.
        let mut end = 0;
        for section in &object.sections {
            assert_eq!(section.fixups.start, end);
            end = section.fixups.end;
        }
        assert_eq!(end as usize, object.fixups.len());

        let symbol = |name: &str| object.symbols.iter().find(|it| object.name(it.name) == name).unwrap();
        assert_eq!(object.symbols.len(), 11);
        assert_eq!(
            symbol("_start").def,
            SymbolDef::Section { section: 0, value: 0, size: start.len() as u64 }
        );
        assert_eq!(symbol("init").binding, Binding::Local);
        let fd_write = symbol("__imported_wasi_snapshot_preview1_fd_write");
        assert_eq!(fd_write.def, SymbolDef::Undefined);
        assert_eq!(object.imports.len(), 1);
        let (at, import) = &object.imports[0];
        assert_eq!(object.symbols[*at as usize], *fd_write);
        assert_eq!((object.name(import.module), object.name(import.field)), ("wasi_snapshot_preview1", "fd_write"));
        // The type is borrowed from the type section, in place.
        let ty = import.type_bytes(&input).unwrap();
        assert_eq!(ty, [0x60, 4, 0x7f, 0x7f, 0x7f, 0x7f, 1, 0x7f]);
        let ty = Signature::decode(ty, &mut 0).unwrap();
        assert_eq!(ty.params.len(), 4);
        assert_eq!(ty.results, [ValType::I32]);
        assert_eq!(object.name(object.symbols[SP as usize].name), "__stack_pointer");
        assert_eq!(object.symbols[SP as usize].kind, SymbolKind::Other);
        assert_eq!(object.name(object.symbols[8].name), "__indirect_function_table");
        assert_eq!(symbol("msg").def, SymbolDef::Section { section: 2, value: 0, size: 4 });
        assert!(symbol("buf").hidden);
        assert_eq!(symbol("extern_data").def, SymbolDef::Undefined);
        assert_eq!(object.symbols[9].kind, SymbolKind::Section);
        assert_eq!(object.symbols[9].def, SymbolDef::Section { section: 7, value: 0, size: 0 });
        assert_eq!(object.groups.len(), 1);
        assert_eq!(object.name(object.groups[0].name), "grp");
        assert!(!object.groups[0].largest);
        assert_eq!(object.group_sections(&object.groups[0]), [1, 4]);
    }

    /// An object of `functions` functions `f0`, `f1`, ... of type `() -> ()`, each calling the
    /// imported `fd_write`: a relocation and a symbol each.
    fn object_of(functions: usize) -> Vec<u8> {
        let mut out = reader::MAGIC.to_vec();
        section(&mut out, 1, &vec_of(1, &[0x60, 0, 0]));
        let mut imports = Vec::new();
        name(&mut imports, "wasi_snapshot_preview1");
        name(&mut imports, "fd_write");
        imports.extend([0, 0]);
        section(&mut out, 2, &vec_of(1, &imports));
        section(&mut out, 3, &vec_of(functions, &vec![0; functions]));
        // Each body: no locals, `call` and a padded index, `end`.
        let body = [0x00, 0x10, 0x80, 0x80, 0x80, 0x80, 0x00, 0x0b];
        let mut code = Vec::new();
        uleb(&mut code, functions as u64);
        let first = code.len();
        let mut relocs = Vec::with_capacity(functions);
        for at in 0..functions {
            relocs.push((0, (first + at * (1 + body.len()) + 3) as u32, 0, 0));
            code.push(body.len() as u8);
            code.extend(body);
        }
        section(&mut out, 10, &code);

        let mut linking = Vec::new();
        uleb(&mut linking, LINKING_VERSION.into());
        let mut symbols = vec![SYM_FUNCTION, (UNDEFINED | EXPLICIT_NAME) as u8, 0];
        name(&mut symbols, "fd_write");
        for at in 0..functions {
            symbols.extend([SYM_FUNCTION, 0]);
            uleb(&mut symbols, at as u64 + 1);
            name(&mut symbols, &format!("f{at}"));
        }
        subsection(&mut linking, SYMBOL_TABLE, &vec_of(functions + 1, &symbols));
        custom(&mut out, "linking", &linking);
        // The sections: type 0, import 1, function 2, code 3.
        reloc_section(&mut out, "reloc.CODE", 3, &relocs);
        out
    }

    #[test]
    fn an_object_is_read_in_a_constant_number_of_buffers() {
        let used = |functions| {
            let input = object_of(functions);
            let (object, used) = blaze_alloc_count::measure(|| read_object(&input).unwrap());
            assert_eq!(object.sections.len(), functions);
            assert_eq!(object.fixups.len(), functions);
            assert_eq!(object.symbols.len(), functions + 1);
            assert_eq!(object.imports.len(), 1);
            used
        };
        let (small, large) = (used(100), used(2000));
        // About 30: the reader's columns (sections, types, imports, function types, bodies,
        // customs, relocation sections, symbols), the object's (sections, fixups, symbols,
        // names, imports, the bytes the reader made), the relocations and their runs, the
        // functions' names, the code's relocations by body and the sort's scratch, the reused
        // buffers, and the one import's names and type. None per function, relocation or symbol.
        assert!(small.allocations <= 40, "{small:?}");
        assert_eq!(large.allocations, small.allocations, "20 times the functions");
    }

    #[test]
    fn refuses_what_it_does_not_know() {
        let mut relocs = code_relocs();
        relocs[1].0 = 99;
        let error = read_object(&object_bytes(&relocs, None)).unwrap_err();
        assert!(error.contains("crt1.o") && error.contains("99"), "{error}");

        let mut relocs = code_relocs();
        relocs[1].0 = 21;
        let error = read_object(&object_bytes(&relocs, None)).unwrap_err();
        assert!(error.contains("R_WASM_MEMORY_ADDR_TLS_SLEB"), "{error}");

        let mut relocs = code_relocs();
        relocs[0].1 = 1;
        let error = read_object(&object_bytes(&relocs, None)).unwrap_err();
        assert!(error.contains("no function body"), "{error}");

        let error = read_object(&object_bytes(&code_relocs(), Some(&[SYM_TAG, 0, 0, 1, b't'])))
            .unwrap_err();
        assert!(error.contains("exception tag"), "{error}");

        let error = read_object(&object_bytes(&code_relocs(), Some(&[9, 0]))).unwrap_err();
        assert!(error.contains("unknown kind 9"), "{error}");

        let mut linked = reader::MAGIC.to_vec();
        section(&mut linked, 1, &[0]);
        let error = read_object(&linked).unwrap_err();
        assert!(error.contains("no linking section"), "{error}");

        assert!(read("libc.so", &linked, &NativeKind::Shared { as_needed: true }).is_err());
    }

    fn header(name: &str, size: usize) -> Vec<u8> {
        let mut out = format!("{name:<16}{:<12}{:<6}{:<6}{:<8}{size:<10}", 0, 0, 0, 644).into_bytes();
        out.extend(b"`\n");
        assert_eq!(out.len(), 60);
        out
    }

    fn archive_bytes(index: bool) -> Vec<u8> {
        let member = reader::MAGIC.to_vec();
        let names = b"_start\0helper\0";
        let table_len = 4 + 4 * 2 + names.len();
        let first = 8 + if index { 60 + table_len } else { 0 };
        let second = first + 60 + member.len();
        let mut out = b"!<arch>\n".to_vec();
        if index {
            out.extend(header("/", table_len));
            out.extend(2u32.to_be_bytes());
            out.extend((first as u32).to_be_bytes());
            out.extend((second as u32).to_be_bytes());
            out.extend(names);
        }
        out.extend(header("a.o/", member.len()));
        out.extend(&member);
        out.extend(header("b.o/", member.len()));
        out.extend(&member);
        out
    }

    #[test]
    fn reads_an_archive() {
        let bytes = archive_bytes(true);
        let NativeFile::Archive(archive) =
            read("libc.a", &bytes, &NativeKind::Archive { whole: false }).unwrap()
        else {
            panic!("not an archive")
        };
        let names: Vec<&str> = archive.members.iter().map(|it| archive.name(it.name)).collect();
        assert_eq!(names, ["a.o", "b.o"]);
        assert_eq!(&bytes[archive.members[1].bytes.clone()], reader::MAGIC);
        let index: Vec<(&str, u32)> = archive.index.iter().map(|&(name, at)| (archive.name(name), at)).collect();
        assert_eq!(index, [("_start", 0), ("helper", 1)]);
    }

    #[test]
    fn an_archive_without_an_index() {
        let bytes = archive_bytes(false);
        let error = read("libx.a", &bytes, &NativeKind::Archive { whole: false }).err().unwrap();
        assert!(error.contains("libx.a") && error.contains("index"), "{error}");
        assert!(read("libx.a", &bytes, &NativeKind::Archive { whole: true }).is_ok());
    }

    #[test]
    fn a_member_past_the_end() {
        // One header of a 100-byte member, and no bytes after it.
        let mut bytes = b"!<arch>\n".to_vec();
        bytes.extend(header("a.o/", 100));
        let error = read("liba.a", &bytes, &NativeKind::Archive { whole: true }).err().unwrap();
        assert!(error.contains("`a.o`") && error.contains("past the archive"), "{error}");
    }

    /// A member of `bsd`'s archive: `#1/<len>`, the name padded with NULs, then `data`.
    fn bsd_member(out: &mut Vec<u8>, name: &str, data: &[u8]) {
        let padded = (name.len() + 1).next_multiple_of(8);
        out.extend(header(&format!("#1/{padded}"), padded + data.len()));
        out.extend(name.as_bytes());
        out.resize(out.len() + padded - name.len(), 0);
        out.extend(data);
        if out.len() % 2 == 1 {
            out.push(b'\n');
        }
    }

    #[test]
    fn a_bsd_index_names_one_string_many_times() {
        // 640 entries of a `__.SYMDEF`, every one naming the same 4096-byte string.
        let mut strings = vec![b'a'; 4096];
        strings.push(0);
        strings.resize(strings.len().next_multiple_of(4), 0);
        let entries = 640;
        let symdef_len = 4 + 8 * entries + 4 + strings.len();
        let mut member = 8 + 60 + 16 + symdef_len;
        member += member % 2;
        let mut symdef = (8 * entries as u32).to_le_bytes().to_vec();
        for _ in 0..entries {
            symdef.extend(0u32.to_le_bytes());
            symdef.extend((member as u32).to_le_bytes());
        }
        symdef.extend((strings.len() as u32).to_le_bytes());
        symdef.extend(&strings);
        let mut bytes = b"!<arch>\n".to_vec();
        bsd_member(&mut bytes, "__.SYMDEF", &symdef);
        assert_eq!(bytes.len(), member);
        bsd_member(&mut bytes, "a.o", reader::MAGIC);

        let NativeFile::Archive(archive) =
            read("libb.a", &bytes, &NativeKind::Archive { whole: false }).unwrap()
        else {
            panic!("not an archive")
        };
        assert_eq!(archive.index.len(), entries);
        assert!(archive.index.iter().all(|it| *it == archive.index[0]));
        assert_eq!(archive.name(archive.index[0].0), "a".repeat(4096));
        // The string once, and the member's name.
        let member = archive.name(archive.members[0].name).len();
        assert_eq!(archive.names.bytes(), 4096 + member);
    }
}
