//! Thin Mach-O dynamic libraries: the install name and the exports, from the export trie (or,
//! without one, the symbol table's external definitions).

#![forbid(unsafe_code)]

use blaze_codegen::Arch;
use blaze_format::cursor::{self, Reader, Wanted};
use blaze_format::leb::LebError;
use blaze_ids::{bits::BitSet, radix};
use object::Endianness;
use object::macho;
use object::read::macho::{MachHeader as _, Nlist as _, Section as _, Segment as _, SymbolTable};
use smallvec::SmallVec;

use crate::model::{Names, Shared, SharedSymbol, SymbolKind};

/// A library's sections, kept inline: system dylibs have a few dozen.
type SectionList = SmallVec<[Section; 32]>;

/// The trie walk's name buffer and edge stack, kept inline: past these the walk spills to the
/// heap once and doubles from there.
const NAME_INLINE: usize = 256;
const EDGES_INLINE: usize = 64;

type Header = macho::MachHeader64<Endianness>;

/// A thin Mach-O dynamic library (`MH_DYLIB`, or an `MH_DYLIB_STUB`) for `arch`: its install
/// name (`LC_ID_DYLIB`) and what it exports.
pub(super) fn read(name: &str, bytes: &[u8], arch: Arch) -> Result<Shared, String> {
    // alloc: error path
    read_dylib(bytes, arch).map_err(|what| format!("{name}: {what}"))
}

fn err(error: object::read::Error) -> String {
    // alloc: error path
    error.to_string()
}

#[derive(Clone, Copy)]
struct Section {
    addr: u64,
    size: u64,
    flags: u32,
}

impl Section {
    fn contains(&self, addr: u64) -> bool {
        addr >= self.addr && addr - self.addr < self.size
    }

    fn code(&self) -> bool {
        self.flags & (macho::S_ATTR_PURE_INSTRUCTIONS | macho::S_ATTR_SOME_INSTRUCTIONS).0 != 0
    }
}

fn read_dylib(bytes: &[u8], arch: Arch) -> Result<Shared, String> {
    let magic = bytes
        .get(..4)
        .map(|it| u32::from_be_bytes([it[0], it[1], it[2], it[3]]))
        .ok_or("too short for a Mach-O header")?;
    match magic {
        macho::MH_MAGIC_64 | macho::MH_CIGAM_64 => {}
        macho::FAT_MAGIC | macho::FAT_CIGAM | macho::FAT_MAGIC_64 | macho::FAT_CIGAM_64 => {
            // alloc: error path
            return Err("a universal (fat) file: only a thin slice is read here".to_owned());
        }
        // alloc: error path
        macho::MH_MAGIC | macho::MH_CIGAM => return Err("a 32-bit Mach-O".to_owned()),
        // alloc: error path
        _ => return Err(format!("not a Mach-O file (magic {magic:#010x})")),
    }
    let header = Header::parse(bytes, 0).map_err(err)?;
    let endian = header.endian().map_err(err)?;
    let want = match arch {
        Arch::Aarch64 => macho::CPU_TYPE_ARM64,
        Arch::X86_64 => macho::CPU_TYPE_X86_64,
        // alloc: error path
        Arch::Wasm32 => return Err("a Mach-O library in a wasm32 link".to_owned()),
    };
    let cpu = header.cputype(endian);
    if cpu != want {
        // alloc: error path
        return Err(format!("a Mach-O of CPU type {cpu:#x}, not {arch:?} ({want:#x})"));
    }
    let filetype = header.filetype(endian);
    if filetype != macho::MH_DYLIB && filetype != macho::MH_DYLIB_STUB {
        // alloc: error path
        return Err(format!("not a dynamic library (Mach-O file type {filetype})"));
    }

    let mut install_name = None;
    let mut inputs = Vec::new(); // alloc: empty unless the library re-exports; `Shared::inputs`
    let mut trie: Option<(&'static str, u32, u32)> = None;
    let mut symtab = None;
    let mut sections = SectionList::new();
    let mut base = None;
    let mut commands = header.load_commands(endian, bytes, 0).map_err(err)?;
    while let Some(command) = commands.next().map_err(err)? {
        match command.cmd() {
            macho::LC_ID_DYLIB | macho::LC_REEXPORT_DYLIB => {
                let dylib: &macho::DylibCommand<Endianness> = command.data().map_err(err)?;
                let path = command.string(endian, dylib.dylib.name).map_err(err)?;
                let path = std::str::from_utf8(path)
                    // alloc: error path
                    .map_err(|_| "a dylib path that is not UTF-8".to_owned())?
                    // alloc: the install name and re-exports are `Shared`'s `String`s: once per command naming one
                    .to_owned();
                if command.cmd() == macho::LC_ID_DYLIB {
                    if install_name.replace(path).is_some() {
                        // alloc: error path
                        return Err("two LC_ID_DYLIB commands".to_owned());
                    }
                } else if !inputs.contains(&path) {
                    inputs.push(path);
                }
            }
            macho::LC_DYLD_INFO | macho::LC_DYLD_INFO_ONLY => {
                let info: &macho::DyldInfoCommand<Endianness> = command.data().map_err(err)?;
                let size = info.export_size.get(endian);
                if size != 0 {
                    set_trie(&mut trie, "LC_DYLD_INFO", info.export_off.get(endian), size)?;
                }
            }
            macho::LC_DYLD_EXPORTS_TRIE => {
                let data: &macho::LinkeditDataCommand<Endianness> = command.data().map_err(err)?;
                let size = data.datasize.get(endian);
                if size != 0 {
                    set_trie(&mut trie, "LC_DYLD_EXPORTS_TRIE", data.dataoff.get(endian), size)?;
                }
            }
            macho::LC_SYMTAB => symtab = command.symtab().map_err(err)?,
            macho::LC_SEGMENT_64 => {
                if let Some((segment, data)) = command.segment_64().map_err(err)? {
                    if segment.fileoff(endian) == 0 && segment.filesize(endian) != 0 {
                        base.get_or_insert(segment.vmaddr(endian));
                    }
                    for section in segment.sections(endian, data).map_err(err)? {
                        sections.push(Section {
                            addr: section.addr(endian),
                            size: section.size(endian),
                            flags: section.flags(endian).0,
                        });
                    }
                }
            }
            _ => {}
        }
    }
    let name = install_name.ok_or("a dynamic library without LC_ID_DYLIB")?;

    let mut names;
    let symbols = match trie {
        Some((command, offset, size)) => {
            let data = usize::try_from(offset)
                .ok()
                .zip(usize::try_from(size).ok())
                .and_then(|(offset, size)| bytes.get(offset..offset.checked_add(size)?))
                .ok_or_else(|| {
                    // alloc: error path
                    format!("{command}'s export trie ({size} bytes at {offset:#x}) is outside the file")
                })?;
            let base = base.ok_or("an export trie, but no segment maps the Mach-O header")?;
            // The trie shares prefixes, so its names take more bytes than it does: start at
            // twice its size; past that the arena doubles, a few times per file at most.
            names = Names::with_capacity(data.len() * 2);
            trie_exports(data, base, &sections, &mut names)
                // alloc: error path
                .map_err(|what| format!("{command}'s export trie: {what}"))?
        }
        None => {
            let symtab = symtab.ok_or("neither an export trie nor a symbol table")?;
            let table: SymbolTable<'_, Header, &[u8]> = symtab.symbols(endian, bytes).map_err(err)?;
            // The string table holds every name, each of which loses its `_` (or, rarely, gains a
            // `\x01`). `symbols` checks only the nlists: a name read past the file's end is an
            // error, so only the table's bytes inside the file can be names.
            let (stroff, strsize) = (symtab.stroff.get(endian) as usize, symtab.strsize.get(endian) as usize);
            names = Names::with_capacity(strsize.min(bytes.len().saturating_sub(stroff)));
            symtab_exports(&table, endian, &sections, &mut names)?
        }
    };
    // alloc: empty `Vec`s, which don't allocate
    Ok(Shared { name, symbols, names, versions: Vec::new(), more: Vec::new(), inputs })
}

fn set_trie(
    trie: &mut Option<(&'static str, u32, u32)>,
    command: &'static str,
    offset: u32,
    size: u32,
) -> Result<(), String> {
    if let Some((other, ..)) = trie.replace((command, offset, size)) {
        // alloc: error path
        return Err(format!("two export tries ({other} and {command})"));
    }
    Ok(())
}

/// A trie read's error in the words `malformed_tries` checks, the same from both walks.
fn trie_error(error: cursor::Error) -> String {
    let at = error.at;
    match error.wanted {
        Wanted::Leb(LebError::Truncated) => format!("a ULEB128 at {at:#x} runs past the end"), // alloc: error path
        Wanted::Leb(LebError::Overflow) => format!("a ULEB128 at {at:#x} is too long"), // alloc: error path
        Wanted::Nul { .. } => format!("a string at {at:#x} runs past the end"), // alloc: error path
        _ => error.into(),
    }
}

/// The sections by address, to classify an export's address in O(log n).
struct Sections<'a> {
    /// In load-command order.
    all: &'a [Section],
    /// The non-empty ones by address, when no two overlap (as every linker writes them):
    /// then at most one holds an address, the one `all`'s first match finds.
    sorted: Option<SectionList>,
}

impl<'a> Sections<'a> {
    fn new(all: &'a [Section]) -> Sections<'a> {
        let mut sorted = SectionList::new();
        sorted.extend(all.iter().copied().filter(|it| it.size != 0));
        radix::sort_copy_by_key(&mut sorted, |it| it.addr);
        let disjoint = sorted.windows(2).all(|it| it[0].addr.checked_add(it[0].size).is_some_and(|end| end <= it[1].addr));
        Sections { all, sorted: disjoint.then_some(sorted) }
    }

    fn find(&self, addr: u64) -> Option<&Section> {
        match &self.sorted {
            Some(sorted) => {
                let after = sorted.partition_point(|it| it.addr <= addr);
                after.checked_sub(1).map(|it| &sorted[it]).filter(|it| it.contains(addr))
            }
            None => self.all.iter().find(|it| it.contains(addr)),
        }
    }
}

/// An edge of the trie still to walk: its label (`len` bytes at `label`), the child it leads
/// to, and the name's length at its parent. 16 bytes; one stack of them for the whole walk.
#[derive(Clone, Copy)]
struct Edge {
    label: u32,
    len: u32,
    child: u32,
    parent: u32,
}

const _: () = assert!(size_of::<Edge>() == 16);

/// Every terminal node of the trie, in the order of a walk that takes edges as written (each
/// node's edges are read before its first child is entered). The name is one buffer cut back
/// to each edge's parent, the nodes seen are a bit per trie byte, and each export's name is
/// appended to `names`.
fn trie_exports(
    data: &[u8],
    base: u64,
    sections: &[Section],
    names: &mut Names,
) -> Result<Vec<SharedSymbol>, String> {
    // `LC_DYLD_INFO`'s and `LC_DYLD_EXPORTS_TRIE`'s sizes are `u32`, so offsets and names fit.
    debug_assert!(u32::try_from(data.len()).is_ok());
    let sections = Sections::new(sections);
    let classify = |offset: u64| -> SymbolKind {
        match sections.find(base.wrapping_add(offset)) {
            Some(section) if section.code() => SymbolKind::Function,
            _ => SymbolKind::Data,
        }
    };
    // A node takes at least 4 bytes (its info's size, flags and address, its child count) and
    // the edge to it at least 2 (an empty label's NUL, its offset): at most one export per 6
    // bytes of a trie whose nodes don't overlap, so `out` is sized once.
    let mut out = Vec::with_capacity(data.len() / 6 + 1);
    let mut seen = BitSet::<usize>::new(data.len()); // alloc: one bitset per trie, sized exactly
    let mut name: SmallVec<[u8; NAME_INLINE]> = SmallVec::new();
    let mut stack: SmallVec<[Edge; EDGES_INLINE]> = SmallVec::new();
    let mut node = 0usize;
    loop {
        let mut r = Reader::starting_at(data, node).map_err(trie_error)?;
        let terminal = r.uleb().map_err(trie_error)?;
        // A byte was read at `node`, so it is inside `seen` (the root of an empty trie is not).
        if !seen.insert(node) {
            // alloc: error path
            return Err(format!("node {node:#x} is reached twice (a cycle)"));
        }
        if terminal != 0 {
            let info = usize::try_from(terminal)
                .ok()
                .and_then(|it| r.take(it).ok())
                // alloc: error path
                .ok_or_else(|| format!("node {node:#x}'s export info runs past the end"))?;
            let (kind, weak) = export(info, &classify)
                // alloc: error path
                .map_err(|what| format!("node {node:#x}: {what}"))?;
            out.push(SharedSymbol { name: super::push_core_name(names, &name)?, version: None, kind, weak });
        }
        // alloc: error path
        let count = r.u8().map_err(|_| format!("node {node:#x} runs past the end"))?;
        let first = stack.len();
        for _ in 0..count {
            let label = r.at();
            let len = r.cstr().map_err(trie_error)?.len();
            let child = r.uleb().map_err(trie_error)?;
            let child = usize::try_from(child)
                .ok()
                .filter(|&it| it < data.len())
                // alloc: error path
                .ok_or_else(|| format!("node {node:#x} has a child at {child:#x}, out of range"))?;
            stack.push(Edge { label: label as u32, len: len as u32, child: child as u32, parent: name.len() as u32 });
        }
        stack[first..].reverse();
        let Some(edge) = stack.pop() else { break };
        name.truncate(edge.parent as usize);
        name.extend_from_slice(&data[edge.label as usize..][..edge.len as usize]);
        node = edge.child as usize;
    }
    Ok(out)
}

/// An export as the reader held it before names moved into the arena: the form the tests
/// and the reference walk compare in.
#[cfg(any(test, feature = "fuzz"))]
#[derive(PartialEq, Eq, Debug)]
struct Owned {
    name: String,
    kind: SymbolKind,
    weak: bool,
}

/// The walk as it was before the one-buffer walk: a stack of owned prefixes, a hash set
/// of the nodes seen, the model `trie_exports` is checked against.
#[cfg(any(test, feature = "fuzz"))]
fn reference_trie_exports(data: &[u8], base: u64, sections: &[Section]) -> Result<Vec<Owned>, String> {
    let classify = |offset: u64| -> SymbolKind {
        let addr = base.wrapping_add(offset);
        match sections.iter().find(|it| it.contains(addr)) {
            Some(section) if section.code() => SymbolKind::Function,
            _ => SymbolKind::Data,
        }
    };
    // alloc: the reference walk, for tests and the fuzz differential only
    let mut out = Vec::new();
    let mut visited = blaze_hash::HashSet::default();
    // alloc: the reference walk, for tests and the fuzz differential only
    let mut stack = vec![(0usize, Vec::new())];
    while let Some((node, prefix)) = stack.pop() {
        if !visited.insert(node) {
            // alloc: the reference walk's error
            return Err(format!("node {node:#x} is reached twice (a cycle)"));
        }
        let mut r = Reader::starting_at(data, node).map_err(trie_error)?;
        let terminal = r.uleb().map_err(trie_error)?;
        if terminal != 0 {
            let info = usize::try_from(terminal)
                .ok()
                .and_then(|it| r.take(it).ok())
                // alloc: the reference walk's error
                .ok_or_else(|| format!("node {node:#x}'s export info runs past the end"))?;
            // alloc: the reference walk's error
            let (kind, weak) = export(info, &classify).map_err(|what| format!("node {node:#x}: {what}"))?;
            out.push(Owned { name: super::core_name(&prefix)?, kind, weak });
        }
        // alloc: the reference walk's error
        let count = r.u8().map_err(|_| format!("node {node:#x} runs past the end"))?;
        let mut children = Vec::with_capacity(usize::from(count));
        for _ in 0..count {
            let label = r.cstr().map_err(trie_error)?;
            let child = r.uleb().map_err(trie_error)?;
            let child = usize::try_from(child)
                .ok()
                .filter(|&it| it < data.len())
                // alloc: the reference walk's error
                .ok_or_else(|| format!("node {node:#x} has a child at {child:#x}, out of range"))?;
            // clone: the reference walk's owned prefixes, which the real walk does without
            let mut name = prefix.clone();
            name.extend_from_slice(label);
            children.push((child, name));
        }
        stack.extend(children.into_iter().rev());
    }
    Ok(out)
}

#[cfg(any(test, feature = "fuzz"))]
fn test_sections() -> Vec<Section> {
    let code = (macho::S_ATTR_PURE_INSTRUCTIONS | macho::S_ATTR_SOME_INSTRUCTIONS).0;
    // alloc: sections for tests and the fuzz differential only
    vec![
        Section { addr: 0x4000, size: 0x100, flags: 0 },
        Section { addr: 0x1000, size: 0x100, flags: code },
        Section { addr: 0x4100, size: 0x30, flags: macho::S_THREAD_LOCAL_VARIABLES.to_flags().0 },
        Section { addr: 0x2000, size: 0, flags: code },
    ]
}

/// [`trie_exports`] against [`reference_trie_exports`] (`crate::fuzz_trie_differential`).
#[cfg(feature = "fuzz")]
pub(crate) fn trie_differential(trie: &[u8]) -> Result<bool, String> {
    // A library with a zero-sized trie has none: `read_dylib` never walks an empty one.
    if trie.is_empty() {
        return Ok(false);
    }
    let sections = test_sections();
    let mut names = Names::default();
    let new = trie_exports(trie, 0, &sections, &mut names).map(|symbols| {
        symbols
            .iter()
            // alloc: the fuzz differential only, to compare with the reference's owned names
            .map(|it| Owned { name: names.at(it.name).to_owned(), kind: it.kind, weak: it.weak })
            // alloc: the fuzz differential only
            .collect::<Vec<_>>()
    });
    let old = reference_trie_exports(trie, 0, &sections);
    if new == old {
        Ok(new.is_ok())
    } else {
        // alloc: the fuzz differential's failure message
        Err(format!("trie_exports: {new:?}\nthe reference: {old:?}"))
    }
}

/// What a terminal node's export info says: the symbol's kind and whether it is weak.
fn export(info: &[u8], classify: &impl Fn(u64) -> SymbolKind) -> Result<(SymbolKind, bool), String> {
    let known = macho::EXPORT_SYMBOL_FLAGS_KIND_MASK
        | (macho::EXPORT_SYMBOL_FLAGS_WEAK_DEFINITION
            | macho::EXPORT_SYMBOL_FLAGS_REEXPORT
            | macho::EXPORT_SYMBOL_FLAGS_STUB_AND_RESOLVER
            | macho::EXPORT_SYMBOL_FLAGS_STATIC_RESOLVER)
            .0;
    let mut info = Reader::new(info);
    let flags = info.uleb().map_err(trie_error)?;
    if flags & !known != 0 {
        // alloc: error path
        return Err(format!("unknown export flags {:#x}", flags & !known));
    }
    let weak = flags & macho::EXPORT_SYMBOL_FLAGS_WEAK_DEFINITION.0 != 0;
    let kind = (flags & macho::EXPORT_SYMBOL_FLAGS_KIND_MASK) as u8;
    let tls = kind == macho::EXPORT_SYMBOL_FLAGS_KIND_THREAD_LOCAL.0;
    if kind > macho::EXPORT_SYMBOL_FLAGS_KIND_ABSOLUTE.0 {
        // alloc: error path
        return Err(format!("unknown export kind {kind}"));
    }
    let kind = if flags & macho::EXPORT_SYMBOL_FLAGS_REEXPORT.0 != 0 {
        info.uleb().map_err(trie_error)?;
        info.cstr().map_err(trie_error)?;
        if tls { SymbolKind::Tls } else { SymbolKind::Other }
    } else if flags & macho::EXPORT_SYMBOL_FLAGS_STUB_AND_RESOLVER.0 != 0 {
        let stub = info.uleb().map_err(trie_error)?;
        info.uleb().map_err(trie_error)?;
        classify(stub)
    } else {
        let address = info.uleb().map_err(trie_error)?;
        if tls {
            SymbolKind::Tls
        } else if kind == macho::EXPORT_SYMBOL_FLAGS_KIND_ABSOLUTE.0 {
            SymbolKind::Data
        } else if flags & macho::EXPORT_SYMBOL_FLAGS_STATIC_RESOLVER.0 != 0 {
            SymbolKind::Function
        } else {
            classify(address)
        }
    };
    Ok((kind, weak))
}

/// The symbol table's external definitions, for a library without an export trie, their
/// names appended to `names`.
fn symtab_exports<'a>(
    table: &SymbolTable<'a, Header, &'a [u8]>,
    endian: Endianness,
    sections: &[Section],
    names: &mut Names,
) -> Result<Vec<SharedSymbol>, String> {
    let external = |n_type: macho::SymbolFlags| {
        !n_type.is_stab()
            && n_type.contains(macho::N_EXT)
            && !n_type.contains(macho::N_PEXT)
            && !matches!(n_type.typ(), macho::N_UNDF | macho::N_PBUD)
    };
    let mut out = Vec::with_capacity(table.iter().filter(|it| external(it.n_type())).count());
    for nlist in table.iter() {
        let n_type = nlist.n_type();
        if !external(n_type) {
            continue;
        }
        let kind = match n_type.typ() {
            macho::N_SECT => {
                let index = nlist.n_sect();
                let section = usize::from(index)
                    .checked_sub(1)
                    .and_then(|it| sections.get(it))
                    // alloc: error path
                    .ok_or_else(|| format!("a symbol in section {index}, which does not exist"))?;
                if macho::SectionFlags(section.flags).typ() == macho::S_THREAD_LOCAL_VARIABLES {
                    SymbolKind::Tls
                } else if section.code() {
                    SymbolKind::Function
                } else {
                    SymbolKind::Data
                }
            }
            macho::N_ABS => SymbolKind::Data,
            macho::N_INDR => SymbolKind::Other,
            // alloc: error path
            other => return Err(format!("a symbol of unknown type {other:#x}")),
        };
        let name = nlist.name(endian, table.strings()).map_err(err)?;
        out.push(SharedSymbol {
            name: super::push_core_name(names, name)?,
            version: None,
            kind,
            weak: nlist.n_desc(endian).contains(macho::N_WEAK_DEF),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use blaze_format::cursor::Put;

    use super::*;

    fn name16(out: &mut Vec<u8>, name: &str) {
        let mut field = [0u8; 16];
        field[..name.len()].copy_from_slice(name.as_bytes());
        out.extend_from_slice(&field);
    }

    fn segment(name: &str, vmaddr: u64, fileoff: u64, filesize: u64, sections: &[(&str, u64, u64, u32)]) -> Vec<u8> {
        let mut out = Vec::new();
        out.put_u32(macho::LC_SEGMENT_64.0);
        out.put_u32(72 + 80 * sections.len() as u32);
        name16(&mut out, name);
        out.put_u64(vmaddr);
        out.put_u64(0x4000);
        out.put_u64(fileoff);
        out.put_u64(filesize);
        out.put_u32(7);
        out.put_u32(5);
        out.put_u32(sections.len() as u32);
        out.put_u32(0);
        for &(sect, addr, size, flags) in sections {
            name16(&mut out, sect);
            name16(&mut out, name);
            out.put_u64(addr);
            out.put_u64(size);
            for value in [0, 4, 0, 0, flags, 0, 0, 0] {
                out.put_u32(value);
            }
        }
        out
    }

    fn dylib_command(cmd: u32, path: &str) -> Vec<u8> {
        let size = (24 + path.len() + 1).next_multiple_of(8);
        let mut out = Vec::new();
        out.put_u32(cmd);
        out.put_u32(size as u32);
        for value in [24, 2, 0x10000, 0x10000] {
            out.put_u32(value);
        }
        out.extend_from_slice(path.as_bytes());
        out.resize(size, 0);
        out
    }

    fn dyld_info(export_off: u32, export_size: u32) -> Vec<u8> {
        let mut out = Vec::new();
        out.put_u32(macho::LC_DYLD_INFO_ONLY.0);
        out.put_u32(48);
        for _ in 0..8 {
            out.put_u32(0);
        }
        out.put_u32(export_off);
        out.put_u32(export_size);
        out
    }

    fn symtab_command(symoff: u32, nsyms: u32, stroff: u32, strsize: u32) -> Vec<u8> {
        let mut out = Vec::new();
        for value in [macho::LC_SYMTAB.0, 24, symoff, nsyms, stroff, strsize] {
            out.put_u32(value);
        }
        out
    }

    /// A dylib of `cpu` whose commands `commands(tail)` make, given where `tail` (the
    /// linkedit data) starts in the file.
    fn file(cpu: u32, commands: impl Fn(u32) -> Vec<Vec<u8>>, tail: &[u8]) -> Vec<u8> {
        let sizeofcmds: usize = commands(0).iter().map(Vec::len).sum();
        let offset = (32 + sizeofcmds).next_multiple_of(8);
        let commands = commands(offset as u32);
        let mut out = Vec::new();
        out.put_u32(macho::MH_MAGIC_64);
        out.put_u32(cpu);
        out.put_u32(0);
        out.put_u32(macho::MH_DYLIB.0);
        out.put_u32(commands.len() as u32);
        out.put_u32(sizeofcmds as u32);
        out.put_u32(0);
        out.put_u32(0);
        for command in commands {
            out.extend_from_slice(&command);
        }
        out.resize(offset, 0);
        out.extend_from_slice(tail);
        out
    }

    fn segments(len: u64) -> Vec<Vec<u8>> {
        let code = macho::S_ATTR_PURE_INSTRUCTIONS.0 | macho::S_ATTR_SOME_INSTRUCTIONS.0;
        vec![
            segment("__TEXT", 0, 0, len, &[("__text", 0x1000, 0x100, code)]),
            segment(
                "__DATA",
                0x4000,
                0x4000,
                0,
                &[("__data", 0x4000, 0x100, 0), ("__thread_vars", 0x4100, 0x30, macho::S_THREAD_LOCAL_VARIABLES.to_flags().0)],
            ),
        ]
    }

    fn with_trie(cpu: u32, trie: &[u8]) -> Vec<u8> {
        let size = trie.len() as u32;
        file(
            cpu,
            |tail| {
                let mut commands = segments(0x1000);
                commands.push(dylib_command(macho::LC_ID_DYLIB.0, "/usr/lib/libfoo.dylib"));
                commands.push(dylib_command(macho::LC_REEXPORT_DYLIB.0, "/usr/lib/libbar.dylib"));
                commands.push(dyld_info(tail, size));
                commands
            },
            trie,
        )
    }

    struct Node {
        terminal: Vec<u8>,
        children: Vec<(&'static str, Node)>,
    }

    fn leaf(terminal: Vec<u8>) -> Node {
        Node { terminal, children: Vec::new() }
    }

    fn flatten<'a>(node: &'a Node, out: &mut Vec<(&'a Node, Vec<(&'static str, usize)>)>) -> usize {
        let index = out.len();
        out.push((node, Vec::new()));
        for &(label, ref child) in &node.children {
            let child = flatten(child, out);
            out[index].1.push((label, child));
        }
        index
    }

    /// Lays a trie out as ld64 does: offsets grown until every child offset's ULEB fits.
    fn build_trie(root: &Node) -> Vec<u8> {
        let mut nodes = Vec::new();
        flatten(root, &mut nodes);
        let mut offsets = vec![0u64; nodes.len()];
        loop {
            let mut next = Vec::with_capacity(nodes.len());
            let mut at = 0u64;
            for (node, children) in &nodes {
                next.push(at);
                let len = node.terminal.len() as u64;
                at += blaze_format::leb::len_u64(len) as u64 + len + 1;
                for (label, child) in children {
                    at += label.len() as u64 + 1 + blaze_format::leb::len_u64(offsets[*child]) as u64;
                }
            }
            if next == offsets {
                break;
            }
            offsets = next;
        }
        let mut out = Vec::new();
        for (index, (node, children)) in nodes.iter().enumerate() {
            assert_eq!(out.len() as u64, offsets[index]);
            blaze_format::leb::write_u64(&mut out, node.terminal.len() as u64);
            out.extend_from_slice(&node.terminal);
            out.push(children.len() as u8);
            for (label, child) in children {
                out.extend_from_slice(label.as_bytes());
                out.push(0);
                blaze_format::leb::write_u64(&mut out, offsets[*child]);
            }
        }
        out
    }

    fn info(flags: u64, values: &[u64]) -> Vec<u8> {
        let mut out = Vec::new();
        blaze_format::leb::write_u64(&mut out, flags);
        for &value in values {
            blaze_format::leb::write_u64(&mut out, value);
        }
        out
    }

    fn reexport(flags: u64, ordinal: u64, import: &str) -> Vec<u8> {
        let mut out = info(flags | macho::EXPORT_SYMBOL_FLAGS_REEXPORT.0, &[ordinal]);
        out.extend_from_slice(import.as_bytes());
        out.push(0);
        out
    }

    fn sym(name: &str, kind: SymbolKind, weak: bool) -> Owned {
        Owned { name: name.to_owned(), kind, weak }
    }

    fn owned(names: &Names, symbols: &[SharedSymbol]) -> Vec<Owned> {
        symbols
            .iter()
            .map(|it| {
                assert_eq!(it.version, None);
                Owned { name: names.at(it.name).to_owned(), kind: it.kind, weak: it.weak }
            })
            .collect()
    }

    /// Every export's name is one push of the arena, back to back: the arena holds each name
    /// once and nothing else, and the library has no versions.
    fn arena_is_exact(shared: &Shared) {
        let bytes: usize = shared.symbols.iter().map(|it| shared.names.get(it.name).expect("in the arena").len()).sum();
        assert_eq!(shared.names.bytes(), bytes);
        assert!(shared.versions.is_empty());
    }

    /// `read`'s exports as the reader held them, the arena checked on the way.
    fn exports(shared: &Shared) -> Vec<Owned> {
        arena_is_exact(shared);
        owned(&shared.names, &shared.symbols)
    }

    /// The new walk, its names resolved.
    fn walk(data: &[u8], base: u64, sections: &[Section]) -> Result<Vec<Owned>, String> {
        let mut names = Names::default();
        let symbols = trie_exports(data, base, sections, &mut names)?;
        assert_eq!(names.bytes(), symbols.iter().map(|it| names.at(it.name).len()).sum::<usize>());
        Ok(owned(&names, &symbols))
    }

    fn sample_trie() -> Vec<u8> {
        let foo = Node {
            terminal: info(0, &[0x1010]),
            children: vec![("bar", leaf(info(macho::EXPORT_SYMBOL_FLAGS_WEAK_DEFINITION.0, &[0x4010])))],
        };
        let fo = Node {
            terminal: Vec::new(),
            children: vec![
                ("o", foo),
                ("b", leaf(reexport(0, 1, "_bob"))),
                ("_tls", leaf(info(u64::from(macho::EXPORT_SYMBOL_FLAGS_KIND_THREAD_LOCAL.0), &[0x4100]))),
            ],
        };
        let underscore = Node {
            terminal: Vec::new(),
            children: vec![
                ("fo", fo),
                ("abs", leaf(info(u64::from(macho::EXPORT_SYMBOL_FLAGS_KIND_ABSOLUTE.0), &[0x1234]))),
                ("res", leaf(info(macho::EXPORT_SYMBOL_FLAGS_STUB_AND_RESOLVER.0, &[0x1020, 0x1030]))),
                ("same", leaf(reexport(0, 1, ""))),
            ],
        };
        build_trie(&Node { terminal: Vec::new(), children: vec![("_", underscore)] })
    }

    #[test]
    fn trie_dylib() {
        let bytes = with_trie(macho::CPU_TYPE_ARM64.0, &sample_trie());
        let shared = read("libfoo.dylib", &bytes, Arch::Aarch64).unwrap();
        assert_eq!(shared.name, "/usr/lib/libfoo.dylib");
        assert_eq!(shared.inputs, vec!["/usr/lib/libbar.dylib".to_owned()]);
        assert!(shared.more.is_empty());
        assert_eq!(
            exports(&shared),
            vec![
                sym("foo", SymbolKind::Function, false),
                sym("foobar", SymbolKind::Data, true),
                sym("fob", SymbolKind::Other, false),
                sym("fo_tls", SymbolKind::Tls, false),
                sym("abs", SymbolKind::Data, false),
                sym("res", SymbolKind::Function, false),
                sym("same", SymbolKind::Other, false),
            ]
        );
    }

    #[test]
    fn exports_trie_command() {
        let trie = sample_trie();
        let size = trie.len() as u32;
        let bytes = file(
            macho::CPU_TYPE_X86_64.0,
            |tail| {
                let mut commands = segments(0x1000);
                commands.push(dylib_command(macho::LC_ID_DYLIB.0, "/usr/lib/libfoo.dylib"));
                let mut trie = Vec::new();
                for value in [macho::LC_DYLD_EXPORTS_TRIE.0, 16, tail, size] {
                    trie.put_u32(value);
                }
                commands.push(trie);
                commands
            },
            &trie,
        );
        let shared = read("libfoo.dylib", &bytes, Arch::X86_64).unwrap();
        assert_eq!(shared.symbols.len(), 7);
        assert_eq!(exports(&shared), exports(&read("libfoo.dylib", &with_trie(macho::CPU_TYPE_X86_64.0, &trie), Arch::X86_64).unwrap()));
        assert!(shared.inputs.is_empty());
    }

    /// `macho.rs` hands the slice over where it starts in the input, at any alignment (an
    /// archive member's, a universal file's slice): every offset reads the same.
    #[test]
    fn any_alignment() {
        let trie = with_trie(macho::CPU_TYPE_ARM64.0, &sample_trie());
        let expected = exports(&read("libfoo.dylib", &trie, Arch::Aarch64).unwrap());
        let mut buffer = Vec::with_capacity(trie.len() + 16);
        for shift in 1..16 {
            buffer.clear();
            buffer.resize(shift, 0xa5);
            buffer.extend_from_slice(&trie);
            let shared = read("libfoo.dylib", &buffer[shift..], Arch::Aarch64).unwrap();
            assert_eq!(shared.name, "/usr/lib/libfoo.dylib");
            assert_eq!(exports(&shared), expected, "at {shift}");
        }
    }

    #[test]
    fn wrong_cpu() {
        let bytes = with_trie(macho::CPU_TYPE_ARM64.0, &sample_trie());
        let error = read("libfoo.dylib", &bytes, Arch::X86_64).unwrap_err();
        assert!(error.starts_with("libfoo.dylib: ") && error.contains("CPU type"), "{error}");
    }

    #[test]
    fn malformed_tries() {
        let cycle = [0x00, 0x01, b'a', 0x00, 0x00];
        let out_of_range = [0x00, 0x01, b'a', 0x00, 0x40];
        let mut too_long = vec![0x80; 11];
        too_long.extend_from_slice(&[0x00, 0x00]);
        let runs_past = [0x05, 0x00];
        for (trie, what) in [
            (&cycle[..], "reached twice"),
            (&out_of_range[..], "out of range"),
            (&too_long[..], "too long"),
            (&runs_past[..], "runs past the end"),
        ] {
            let bytes = with_trie(macho::CPU_TYPE_ARM64.0, trie);
            let error = read("bad.dylib", &bytes, Arch::Aarch64).unwrap_err();
            assert!(error.starts_with("bad.dylib: ") && error.contains(what), "{what}: {error}");
        }
    }

    #[test]
    fn symtab_dylib() {
        let symbols: [(&str, u8, u8, u16, u64); 7] = [
            ("_f", macho::N_SECT.0 | macho::N_EXT.0, 1, 0, 0x1000),
            ("_d", macho::N_SECT.0 | macho::N_EXT.0, 2, macho::N_WEAK_DEF.0, 0x4000),
            ("_t", macho::N_SECT.0 | macho::N_EXT.0, 3, 0, 0x4100),
            ("_local", macho::N_SECT.0, 1, 0, 0x1004),
            ("_pext", macho::N_SECT.0 | macho::N_EXT.0 | macho::N_PEXT.0, 1, 0, 0x1008),
            ("_undef", macho::N_UNDF.0 | macho::N_EXT.0, 0, 0, 0),
            ("_abs", macho::N_ABS.0 | macho::N_EXT.0, 0, 0, 0x42),
        ];
        let mut strings = vec![0u8];
        let mut table = Vec::new();
        for (name, n_type, n_sect, n_desc, value) in symbols {
            table.put_u32(strings.len() as u32);
            table.extend_from_slice(&[n_type, n_sect]);
            table.extend_from_slice(&n_desc.to_le_bytes());
            table.put_u64(value);
            strings.extend_from_slice(name.as_bytes());
            strings.push(0);
        }
        let nsyms = symbols.len() as u32;
        let strsize = strings.len() as u32;
        let table_len = table.len() as u32;
        let mut tail = table;
        tail.extend_from_slice(&strings);
        let bytes = file(
            macho::CPU_TYPE_ARM64.0,
            |at| {
                let mut commands = segments(0x1000);
                commands.push(dylib_command(macho::LC_ID_DYLIB.0, "/usr/lib/libsym.dylib"));
                commands.push(symtab_command(at, nsyms, at + table_len, strsize));
                commands
            },
            &tail,
        );
        let shared = read("libsym.dylib", &bytes, Arch::Aarch64).unwrap();
        assert_eq!(shared.name, "/usr/lib/libsym.dylib");
        assert_eq!(
            exports(&shared),
            vec![
                sym("f", SymbolKind::Function, false),
                sym("d", SymbolKind::Data, true),
                sym("t", SymbolKind::Tls, false),
                sym("abs", SymbolKind::Data, false),
            ]
        );
        let expected = exports(&shared);
        for shift in 1..16 {
            let mut buffer = vec![0x5a; shift];
            buffer.extend_from_slice(&bytes);
            let shifted = read("libsym.dylib", &buffer[shift..], Arch::Aarch64).unwrap();
            assert_eq!(exports(&shifted), expected, "at {shift}");
        }
    }

    struct Rng(blaze_ids::rng::SplitMix64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0.next_u64()
        }

        fn below(&mut self, n: usize) -> usize {
            self.0.index(n)
        }
    }

    const LABELS: [&str; 9] = ["_", "a", "b", "foo", "o", "bar", "_x", "\u{e9}", "longer_label"];

    fn random_info(rng: &mut Rng) -> Vec<u8> {
        let addr = [0x1010, 0x4010, 0x4100, 0x9999][rng.below(4)];
        match rng.below(7) {
            0 => Vec::new(),
            1 => info(0, &[addr]),
            2 => info(macho::EXPORT_SYMBOL_FLAGS_WEAK_DEFINITION.0, &[addr]),
            3 => info(u64::from(macho::EXPORT_SYMBOL_FLAGS_KIND_THREAD_LOCAL.0), &[addr]),
            4 => info(u64::from(macho::EXPORT_SYMBOL_FLAGS_KIND_ABSOLUTE.0), &[addr]),
            5 => info(macho::EXPORT_SYMBOL_FLAGS_STUB_AND_RESOLVER.0, &[addr, 0x1030]),
            _ => reexport(0, 1, "_other"),
        }
    }

    fn random_node(rng: &mut Rng, depth: usize) -> Node {
        let children = if depth > 4 { 0 } else { rng.below(4) };
        Node {
            terminal: random_info(rng),
            children: (0..children).map(|_| (LABELS[rng.below(LABELS.len())], random_node(rng, depth + 1))).collect(),
        }
    }

    /// The one-buffer walk against the reference over seeded tries, and over the same tries
    /// with a byte changed or cut off for the error paths.
    #[test]
    fn trie_walk_matches_reference() {
        let mut rng = Rng(blaze_ids::rng::SplitMix64::new(0xd1_1b));
        let sections = test_sections();
        for round in 0..400 {
            let mut trie = build_trie(&random_node(&mut rng, 0));
            match round % 3 {
                1 => {
                    let at = rng.below(trie.len());
                    trie[at] = rng.next() as u8;
                }
                2 => trie.truncate(rng.below(trie.len() + 1)),
                _ => {}
            }
            assert_eq!(walk(&trie, 0, &sections), reference_trie_exports(&trie, 0, &sections), "{trie:x?}");
        }
        let trie = sample_trie();
        assert_eq!(walk(&trie, 0, &sections), reference_trie_exports(&trie, 0, &sections));
    }

    /// Binary search over disjoint sections, and the first match over overlapping ones, against
    /// the first match in load-command order.
    #[test]
    fn sections_find_matches_linear() {
        let mut rng = Rng(blaze_ids::rng::SplitMix64::new(5));
        for _ in 0..200 {
            let all: Vec<Section> = (0..rng.below(6))
                .map(|_| Section {
                    addr: rng.below(16) as u64 * 0x10,
                    size: rng.below(3) as u64 * 0x10,
                    flags: rng.below(2) as u32 * macho::S_ATTR_PURE_INSTRUCTIONS.0,
                })
                .collect();
            let sections = Sections::new(&all);
            for addr in (0..0x120).step_by(8) {
                let linear = all.iter().find(|it| it.contains(addr)).map(|it| (it.addr, it.size, it.flags));
                let found = sections.find(addr).map(|it| (it.addr, it.size, it.flags));
                assert_eq!(found, linear, "{addr:#x}");
            }
        }
        let overlapping = [Section { addr: 0x10, size: 0x20, flags: 0 }, Section { addr: 0, size: 0x20, flags: 0 }];
        assert!(Sections::new(&overlapping).sorted.is_none());
        let high = [Section { addr: u64::MAX - 1, size: 4, flags: 0 }, Section { addr: 0, size: 4, flags: 0 }];
        assert!(Sections::new(&high).find(u64::MAX).is_some());
    }

    /// Reading a library allocates its install name, its one re-export and their list, the
    /// name arena, the symbols and the trie's bitset: nothing per export or per section.
    #[test]
    fn read_allocation_budget() {
        let bytes = with_trie(macho::CPU_TYPE_ARM64.0, &sample_trie());
        let shared = blaze_alloc_count::assert_at_most(6, "dylib::read of a library with a trie", || {
            read("libfoo.dylib", &bytes, Arch::Aarch64)
        })
        .unwrap();
        assert_eq!(shared.symbols.len(), 7);
    }

    /// A string table whose size runs past the file sizes the name arena by the part inside it:
    /// read with no external symbol it is accepted, and with one whose name is past the end it
    /// is refused, neither asking for what the size says.
    #[test]
    fn string_table_past_the_end() {
        let (mut table, strings) = (Vec::new(), b"\0_f\0");
        table.put_u32(1);
        table.extend_from_slice(&[macho::N_SECT.0 | macho::N_EXT.0, 1]);
        table.extend_from_slice(&0u16.to_le_bytes());
        table.put_u64(0x1000);
        for (nsyms, strsize, accepted) in [(0, u32::MAX, true), (0, 0x0100_0000, true), (1, u32::MAX, false)] {
            let table_len = table.len() as u32;
            let mut tail = table.clone();
            tail.extend_from_slice(strings);
            let bytes = file(
                macho::CPU_TYPE_ARM64.0,
                |at| {
                    let mut commands = segments(0x1000);
                    commands.push(dylib_command(macho::LC_ID_DYLIB.0, "/usr/lib/libsym.dylib"));
                    commands.push(symtab_command(at, nsyms, at + table_len, strsize));
                    commands
                },
                &tail,
            );
            let (read, used) = blaze_alloc_count::measure(|| read("libsym.dylib", &bytes, Arch::Aarch64));
            assert_eq!(read.is_ok(), accepted, "{nsyms} symbols, a string table of {strsize:#x} bytes: {read:?}");
            assert!(used.largest <= bytes.len() as u64, "{} bytes at once reading {} bytes", used.largest, bytes.len());
        }
    }

    /// The walk allocates its symbols and its bitset, once each, whatever the trie's size: a
    /// name up to `NAME_INLINE` bytes and up to `EDGES_INLINE` pending edges stay on the stack,
    /// and the names go to the caller's arena, here big enough.
    #[test]
    fn trie_walk_allocation_budget() {
        let sections = test_sections();
        let mut rng = Rng(blaze_ids::rng::SplitMix64::new(0xa11c));
        let wide = Node {
            terminal: Vec::new(),
            children: (0..40).map(|_| (LABELS[rng.below(LABELS.len())], random_node(&mut rng, 3))).collect(),
        };
        for trie in [sample_trie(), build_trie(&random_node(&mut rng, 0)), build_trie(&wide)] {
            let mut sized = Names::default();
            let expected = trie_exports(&trie, 0, &sections, &mut sized).unwrap().len();
            let mut names = Names::with_capacity(sized.bytes());
            let (symbols, used) = blaze_alloc_count::measure(|| trie_exports(&trie, 0, &sections, &mut names));
            assert_eq!(symbols.unwrap().len(), expected);
            assert!(used.allocations <= 2, "{} allocations walking a trie of {} bytes", used.allocations, trie.len());
        }
    }

    /// Classifying an export's address allocates nothing: the sections are kept inline and
    /// sorted in place.
    #[test]
    fn sections_allocate_nothing() {
        let all = test_sections();
        let found = blaze_alloc_count::assert_at_most(0, "Sections::new and Sections::find", || {
            let sections = Sections::new(&all);
            (0..0x5000u64).step_by(8).filter(|&addr| sections.find(addr).is_some()).count()
        });
        assert!(found > 0);
    }
}
