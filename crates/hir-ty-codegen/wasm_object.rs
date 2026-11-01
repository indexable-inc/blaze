//! Writes a relocatable WebAssembly object, the kind LLVM writes and `wasm-ld` links, from
//! the object the code generator builds for every target.
//!
//! The code generator fills an ELF object through `cranelift-object`, whose functions are
//! here [`WasmFunction`]s rather than machine code. This reads that object back and says the
//! same in the wasm object format: each function is a function, each piece of data a
//! segment of its own, each symbol a symbol of the `linking` section, and each relocation
//! one of `reloc.CODE` or `reloc.DATA`. The format is in the tool conventions:
//! <https://github.com/WebAssembly/tool-conventions/blob/main/Linking.md>.
//!
//! `wasm-encoder` writes the sections it knows; the code and data sections are written by
//! hand because relocations are offsets into them, and the reloc sections because it has no
//! encoder for them.

#![forbid(unsafe_code)]

use std::borrow::Cow;

use blaze_hash::HashMap;
use cranelift_object::object::{
    self, Object, ObjectSection, ObjectSymbol, RelocationTarget, SectionIndex, SectionKind,
    SymbolIndex, SymbolKind,
};
use smallvec::SmallVec;
use wasm_encoder::{
    CustomSection, Encode, EntityType, FunctionSection, GlobalType, ImportSection, MemoryType,
    Module, RawSection, RefType, SectionId, TableType, TypeSection, ValType,
};

use super::wasm::{INDEX_LEN, WasmFunction, WasmSignature};

/// The module every import of an object is from.
const ENV: &str = "env";
const STACK_POINTER: &str = "__stack_pointer";
const LINKING_VERSION: u32 = 2;

/// The subsections of the `linking` section.
#[repr(u8)]
enum Subsection {
    SegmentInfo = 5,
    SymbolTable = 8,
}

#[derive(Clone, Copy)]
#[repr(u8)]
enum SymbolType {
    Function = 0,
    Data = 1,
    Global = 2,
}

/// Flags of a symbol.
const BINDING_WEAK: u32 = 1;
const BINDING_LOCAL: u32 = 2;
const VISIBILITY_HIDDEN: u32 = 4;
const UNDEFINED: u32 = 0x10;
/// The symbol's name is not the name it is imported as.
const EXPLICIT_NAME: u32 = 0x40;

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum RelocType {
    FunctionIndexLeb = 0,
    TableIndexSleb = 1,
    TableIndexI32 = 2,
    MemoryAddrSleb = 4,
    MemoryAddrI32 = 5,
    TypeIndexLeb = 6,
    GlobalIndexLeb = 7,
}

impl RelocType {
    fn has_addend(self) -> bool {
        matches!(self, RelocType::MemoryAddrSleb | RelocType::MemoryAddrI32)
    }
}

struct Reloc {
    ty: RelocType,
    /// From the start of the section's contents.
    offset: u32,
    /// A symbol, or a type for [`RelocType::TypeIndexLeb`].
    index: u32,
    addend: i32,
}

/// A function or a piece of data the object defines: where it is in the ELF object and
/// which symbol of the wasm object it is.
struct Defined {
    section: SectionIndex,
    address: u64,
    size: u64,
    symbol: u32,
    /// Where its bytes start in the contents of the code or data section.
    at: u32,
    /// How far into its ELF bytes the bytes at `at` start: past the header of a function.
    skip: u32,
    is_function: bool,
}

#[derive(Default)]
struct Writer {
    types: Vec<WasmSignature>,
    type_of: HashMap<WasmSignature, u32>,
    symbols: Vec<u8>,
    symbol_count: u32,
    code: Vec<u8>,
    code_relocs: Vec<Reloc>,
    data: Vec<u8>,
    data_relocs: Vec<Reloc>,
    segments: Vec<u8>,
    segment_count: u32,
}

const IMPORT: &str = "blaze.import.";

/// The symbol of the function `field` that the module imports from `module`
/// (`wasm_import_module`), rather than from an `env` that some other object defines: the
/// module and the field are read back from it ([`import_of`]), so what is reused from the store
/// refers to the import as what is compiled anew does. Named for its module too, as rustc
/// mangles the name of one: two modules may have imports of the same name.
pub(super) fn import_symbol(module: &str, field: &str) -> String {
    // alloc: the symbol's name, which the caller in `mod.rs` declares
    format!("{IMPORT}{}.{module}{field}", module.len())
}

fn import_of(symbol: &str) -> Option<(&str, &str)> {
    let (len, rest) = symbol.strip_prefix(IMPORT)?.split_once('.')?;
    let len = len.parse().ok()?;
    Some((rest.get(..len)?, rest.get(len..)?))
}

/// Writes as a wasm object what the ELF object `elf` holds. `signatures` has the signature
/// of every function the object names and does not define. `custom` are the contents of
/// custom sections, by name: those of one name are one section, in order, as the linker
/// joins the ones of every object.
pub(super) fn from_elf(
    elf: &[u8],
    signatures: &HashMap<String, WasmSignature>,
    custom: &[(String, Vec<u8>)],
) -> Result<Vec<u8>, String> {
    // alloc: the error's message
    let file = object::File::parse(elf).map_err(|e| format!("wasm object: {e}"))?;
    let mut writer = Writer::default();

    // Imports come first in the index space of functions, so the undefined ones are
    // numbered before any defined one is.
    let mut imports = ImportSection::new();
    imports.import(
        ENV,
        "__linear_memory",
        MemoryType {
            minimum: 0,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        },
    );
    imports.import(
        ENV,
        "__indirect_function_table",
        TableType {
            element_type: RefType::FUNCREF,
            table64: false,
            minimum: 0,
            maximum: None,
            shared: false,
        },
    );
    imports.import(
        ENV,
        STACK_POINTER,
        GlobalType { val_type: ValType::I32, mutable: true, shared: false },
    );
    let stack_pointer = writer.symbol(SymbolType::Global, UNDEFINED, |entry| 0u32.encode(entry));

    // An undefined symbol of an ELF object has no kind: it is a function when the code
    // generator declared one of its name. The ones nothing refers to are left out, as the
    // linker would ask for them.
    let referred: blaze_hash::HashSet<SymbolIndex> = file
        .sections()
        .flat_map(|section| section.relocations())
        .filter_map(|(_, reloc)| match reloc.target() {
            RelocationTarget::Symbol(symbol) => Some(symbol),
            _ => None,
        })
        // alloc: once per object, the set of the symbols relocations name
        .collect();
    let mut symbol_of: HashMap<SymbolIndex, (u32, bool)> = HashMap::default();
    let mut imported = 0u32;
    // alloc: once per object, its functions
    let mut functions = Vec::new();
    // alloc: once per object, its pieces of data
    let mut datas = Vec::new();
    for symbol in file.symbols() {
        // alloc: the error's message
        let name = symbol.name().map_err(|e| format!("wasm object: {e}"))?;
        match (symbol.kind(), symbol.section_index()) {
            (SymbolKind::Text, Some(_)) => functions.push(symbol),
            (SymbolKind::Data | SymbolKind::Tls | SymbolKind::Unknown, Some(_)) => {
                datas.push(symbol)
            }
            (SymbolKind::Section | SymbolKind::File, _) => {}
            (_, None) if !referred.contains(&symbol.index()) => {}
            (_, None) => match signatures.get(name) {
                Some(signature) => {
                    let ty = writer.ty(signature);
                    let explicit = import_of(name);
                    let (module, field) = explicit.unwrap_or((ENV, name));
                    imports.import(module, field, EntityType::Function(ty));
                    let flags = flags(&symbol) | UNDEFINED;
                    let flags = if explicit.is_some() { flags | EXPLICIT_NAME } else { flags };
                    let index = writer.symbol(SymbolType::Function, flags, |entry| {
                        imported.encode(entry);
                        if explicit.is_some() {
                            name.encode(entry);
                        }
                    });
                    imported += 1;
                    symbol_of.insert(symbol.index(), (index, true));
                }
                None => {
                    let flags = flags(&symbol) | UNDEFINED;
                    let index = writer.symbol(SymbolType::Data, flags, |entry| name.encode(entry));
                    symbol_of.insert(symbol.index(), (index, false));
                }
            },
            _ => {}
        }
    }

    let section_data = |index: SectionIndex| {
        // alloc: the error's message
        let section = file.section_by_index(index).map_err(|e| format!("wasm object: {e}"))?;
        // alloc: the error's message
        let data = section.data().map_err(|e| format!("wasm object: {e}"))?;
        Ok::<_, String>((section, data))
    };

    let mut defined = Vec::with_capacity(functions.len() + datas.len());
    let mut function_types = FunctionSection::new();
    (functions.len() as u32).encode(&mut writer.code);
    for (at, symbol) in functions.iter().enumerate() {
        // alloc: the error's message
        let name = symbol.name().map_err(|e| format!("wasm object: {e}"))?;
        let section = symbol.section_index().expect("a defined symbol");
        let (_, data) = section_data(section)?;
        let range = symbol.address() as usize..(symbol.address() + symbol.size()) as usize;
        // alloc: the error's message
        let bytes = data.get(range).ok_or_else(|| format!("wasm object: `{name}` out of range"))?;
        let (function, skip) = WasmFunction::decode(bytes)?;
        function_types.function(writer.ty(&function.signature));
        (function.body.len() as u32).encode(&mut writer.code);
        let start = writer.code.len() as u32;
        writer.code.extend(&function.body);
        for &(offset, ty) in &function.type_fixups {
            let ty = writer.ty(&function.types[ty as usize]);
            writer.fixup(Reloc {
                ty: RelocType::TypeIndexLeb,
                offset: start + offset,
                index: ty,
                addend: 0,
            });
        }
        for &offset in &function.stack_pointer_fixups {
            writer.fixup(Reloc {
                ty: RelocType::GlobalIndexLeb,
                offset: start + offset,
                index: stack_pointer,
                addend: 0,
            });
        }
        let index = imported + at as u32;
        let index = writer.symbol(SymbolType::Function, flags(symbol), |entry| {
            index.encode(entry);
            name.encode(entry);
        });
        symbol_of.insert(symbol.index(), (index, true));
        defined.push(Defined {
            section,
            address: symbol.address(),
            size: symbol.size(),
            symbol: index,
            at: start,
            skip: skip as u32,
            is_function: true,
        });
    }

    // Each piece of data is a segment of its own, at an address of its own, so that the
    // linker drops the ones nothing refers to.
    (datas.len() as u32).encode(&mut writer.data);
    let mut address = 0u32;
    for symbol in &datas {
        // alloc: the error's message
        let name = symbol.name().map_err(|e| format!("wasm object: {e}"))?;
        let section_index = symbol.section_index().expect("a defined symbol");
        let (section, data) = section_data(section_index)?;
        let size = symbol.size() as usize;
        let uninitialized = matches!(
            section.kind(),
            SectionKind::UninitializedData | SectionKind::UninitializedTls
        );
        // `None` for `size` zeros.
        let bytes = if uninitialized {
            None
        } else {
            let start = (symbol.address() - section.address()) as usize;
            let bytes = data
                .get(start..start + size)
                // alloc: the error's message
                .ok_or_else(|| format!("wasm object: `{name}` out of range"))?;
            Some(bytes)
        };
        // Writable data that is all zeros, with no address written into it, is `.bss` as LLVM
        // makes it: the linker leaves such segments out of the module, memory being zeros.
        let zeroed = uninitialized
            || section.kind() == SectionKind::Data
                && section.relocations().next().is_none()
                && bytes.is_some_and(|bytes| bytes.iter().all(|&b| b == 0));
        // Each piece of data is in a section of its own (`per_data_object_section`), which is
        // aligned as the code generator asked. Its size says nothing: four bytes of a field of
        // a value aligned to 64 are aligned to 64.
        if section.address() != symbol.address() || section.size() > symbol.size() {
            // alloc: the error's message
            return Err(format!("wasm object: `{name}` shares its section"));
        }
        let align = section.align().max(1);
        address = address.next_multiple_of(align as u32);
        // An active segment of memory 0, at a constant address.
        0u32.encode(&mut writer.data);
        writer.data.push(0x41);
        (address as i32).encode(&mut writer.data);
        writer.data.push(0x0b);
        (size as u32).encode(&mut writer.data);
        let start = writer.data.len() as u32;
        match bytes {
            Some(bytes) => writer.data.extend_from_slice(bytes),
            None => writer.data.resize(writer.data.len() + size, 0),
        }
        address += size as u32;

        // The messages of the stand-ins keep their section's name, which the linker merges
        // them by into one segment of its own that the module names.
        let (_, stand_ins) = super::stand_in_section(::object::BinaryFormat::Wasm);
        let (prefix, segment_name) = match section.kind() {
            _ if section.name() == Ok(stand_ins) => (stand_ins, ""),
            _ if zeroed => (".bss.", name),
            SectionKind::ReadOnlyData | SectionKind::ReadOnlyString => (".rodata.", name),
            _ => (".data.", name),
        };
        // The name as `str` encodes it: its length, then its bytes.
        (prefix.len() + segment_name.len()).encode(&mut writer.segments);
        writer.segments.extend_from_slice(prefix.as_bytes());
        writer.segments.extend_from_slice(segment_name.as_bytes());
        align.trailing_zeros().encode(&mut writer.segments);
        0u32.encode(&mut writer.segments);
        let segment = writer.segment_count;
        writer.segment_count += 1;

        let index = writer.symbol(SymbolType::Data, flags(symbol), |entry| {
            name.encode(entry);
            segment.encode(entry);
            0u32.encode(entry);
            (size as u32).encode(entry);
        });
        symbol_of.insert(symbol.index(), (index, false));
        defined.push(Defined {
            section: section_index,
            address: symbol.address(),
            size: symbol.size(),
            symbol: index,
            at: start,
            skip: 0,
            is_function: false,
        });
    }

    blaze_ids::radix::sort_by_key(&mut defined, |it| blaze_ids::radix::pack_64(it.section.0 as u64, it.address));
    let containing = |section: SectionIndex, address: u64| {
        let after =
            defined.partition_point(|it| (it.section.0, it.address) <= (section.0, address));
        let found = &defined[after.checked_sub(1)?];
        // The address one past a symbol is the next one's, except past the last.
        (found.section == section && address <= found.address + found.size).then_some(found)
    };
    for section in file.sections() {
        for (offset, reloc) in section.relocations() {
            let Some(place) = containing(section.index(), offset)
                .filter(|it| offset < it.address + it.size.max(1))
            else {
                // Of a section that is not carried over: unwind tables.
                continue;
            };
            let RelocationTarget::Symbol(target) = reloc.target() else {
                // alloc: the error's message
                return Err("wasm object: relocation against no symbol".to_owned());
            };
            let target_symbol =
                // alloc: the error's message
                file.symbol_by_index(target).map_err(|e| format!("wasm object: {e}"))?;
            let mut addend = reloc.addend();
            // A relocation against a section is against what is at the addend in it.
            let (symbol, is_function) = if target_symbol.kind() == SymbolKind::Section {
                let within = target_symbol
                    .section_index()
                    .and_then(|section| containing(section, addend as u64))
                    .ok_or("wasm object: relocation into no symbol")?;
                addend -= within.address as i64;
                (within.symbol, within.is_function)
            } else {
                *symbol_of
                    .get(&target)
                    // alloc: the error's message
                    .ok_or_else(|| format!("wasm object: relocation against {target_symbol:?}"))?
            };
            let in_code = place.is_function;
            let call = in_code && reloc.kind() != object::RelocationKind::Absolute;
            let ty = match (in_code, is_function) {
                (true, true) if call => RelocType::FunctionIndexLeb,
                (true, true) => RelocType::TableIndexSleb,
                (true, false) => RelocType::MemoryAddrSleb,
                (false, true) => RelocType::TableIndexI32,
                (false, false) => RelocType::MemoryAddrI32,
            };
            if is_function && addend != 0 {
                // alloc: the error's message
                return Err("wasm object: an address inside a function".to_owned());
            }
            let within = (offset - place.address) as u32;
            let Some(within) = within.checked_sub(place.skip) else {
                // alloc: the error's message
                return Err("wasm object: relocation in the header of a function".to_owned());
            };
            let reloc =
                Reloc { ty, offset: place.at + within, index: symbol, addend: addend as i32 };
            if in_code { writer.code_relocs.push(reloc) } else { writer.data_relocs.push(reloc) }
        }
    }

    let mut sections: SmallVec<[(&str, Cow<'_, [u8]>); 4]> = SmallVec::new();
    for (name, bytes) in custom {
        if matches!(name.as_str(), "linking" | "name") || name.starts_with("reloc.") {
            // alloc: the error's message
            return Err(format!("wasm object: a custom section may not be named `{name}`"));
        }
        match sections.iter_mut().find(|(it, _)| it == name) {
            Some((_, contents)) => contents.to_mut().extend(bytes),
            None => sections.push((name, Cow::Borrowed(&bytes[..]))),
        }
    }
    Ok(writer.finish(imports, function_types, &sections))
}

/// The `target_features` section of an object compiled with `enabled` (the `target_feature`
/// values of its crates), which says what its code may use as LLVM's does: the linker joins
/// those of every object, and wasm-bindgen reads `reference-types` off the program's.
pub(super) fn target_features(enabled: impl IntoIterator<Item = String>) -> (String, Vec<u8>) {
    // One bit per known feature, written in `KNOWN_FEATURES`' order: its names' byte order.
    let mut features = 0u32;
    for feature in enabled {
        let implied = match feature.as_str() {
            "bulk-memory" => Some("bulk-memory-opt"),
            "reference-types" => Some("call-indirect-overlong"),
            "relaxed-simd" => Some("simd128"),
            _ => None,
        };
        for name in std::iter::once(feature.as_str()).chain(implied) {
            if let Some(at) = KNOWN_FEATURES.iter().position(|it| *it == name) {
                features |= 1 << at;
            }
        }
    }
    // alloc: once per object, the section's few bytes, which it returns
    let mut contents = Vec::new();
    features.count_ones().encode(&mut contents);
    for (at, &name) in KNOWN_FEATURES.iter().enumerate() {
        if features >> at & 1 != 0 {
            contents.push(b'+');
            name.encode(&mut contents);
        }
    }
    // alloc: once per object, the section's name, which the caller in `mod.rs` keeps
    ("target_features".to_owned(), contents)
}

/// The features LLVM's wasm target knows, of the names rustc gives them too, sorted by bytes.
const KNOWN_FEATURES: &[&str] = &[
    "atomics",
    "bulk-memory",
    "bulk-memory-opt",
    "call-indirect-overlong",
    "exception-handling",
    "extended-const",
    "fp16",
    "gc",
    "multimemory",
    "multivalue",
    "mutable-globals",
    "nontrapping-fptoint",
    "reference-types",
    "relaxed-simd",
    "sign-ext",
    "simd128",
    "tail-call",
    "wide-arithmetic",
];

const _: () = {
    assert!(KNOWN_FEATURES.len() <= 32);
    let mut at = 1;
    while at < KNOWN_FEATURES.len() {
        let (a, b) = (KNOWN_FEATURES[at - 1].as_bytes(), KNOWN_FEATURES[at].as_bytes());
        let mut byte = 0;
        while byte < a.len() && byte < b.len() && a[byte] == b[byte] {
            byte += 1;
        }
        assert!(if byte < a.len() && byte < b.len() { a[byte] < b[byte] } else { a.len() < b.len() });
        at += 1;
    }
};

impl Writer {
    fn ty(&mut self, signature: &WasmSignature) -> u32 {
        if let Some(&index) = self.type_of.get(signature) {
            return index;
        }
        let index = self.types.len() as u32;
        // clone: once per distinct signature of the object, which the type section keeps
        self.types.push(signature.clone());
        // clone: as above, the key its index is found by
        self.type_of.insert(signature.clone(), index);
        index
    }

    /// Adds a relocation of the code section over an index already in place. What the linker
    /// fills in has a value until then: the index in this object.
    fn fixup(&mut self, fixup: Reloc) {
        let place = &mut self.code[fixup.offset as usize..][..INDEX_LEN];
        let value = if fixup.ty == RelocType::TypeIndexLeb { fixup.index } else { 0 };
        place.copy_from_slice(&blaze_format::leb::padded_u32(value));
        self.code_relocs.push(fixup);
    }

    /// Adds a symbol and gives its index. `rest` writes what follows the flags.
    fn symbol(&mut self, ty: SymbolType, flags: u32, rest: impl FnOnce(&mut Vec<u8>)) -> u32 {
        self.symbols.push(ty as u8);
        flags.encode(&mut self.symbols);
        rest(&mut self.symbols);
        self.symbol_count += 1;
        self.symbol_count - 1
    }

    fn finish(
        mut self,
        imports: ImportSection,
        functions: FunctionSection,
        custom: &[(&str, Cow<'_, [u8]>)],
    ) -> Vec<u8> {
        let mut types = TypeSection::new();
        for signature in &self.types {
            types.ty().function(
                signature.params.iter().map(|&it| ValType::from(it)),
                signature.results.iter().map(|&it| ValType::from(it)),
            );
        }

        // A LEB128 of a `u32` is at most five bytes: the version, then each subsection's id,
        // size, count and entries.
        let mut linking =
            Vec::with_capacity(5 + 2 * (1 + 5 + 5) + self.symbols.len() + self.segments.len());
        LINKING_VERSION.encode(&mut linking);
        let mut subsection = |id: Subsection, count: u32, entries: &[u8]| {
            let mut contents = Vec::with_capacity(5 + entries.len());
            count.encode(&mut contents);
            contents.extend(entries);
            linking.push(id as u8);
            contents.encode(&mut linking);
        };
        subsection(Subsection::SymbolTable, self.symbol_count, &self.symbols);
        subsection(Subsection::SegmentInfo, self.segment_count, &self.segments);

        // The reloc sections name the section they are for by its place in the module.
        const CODE_SECTION: u32 = 3;
        const DATA_SECTION: u32 = 4;
        let relocs = |section: u32, relocs: &mut Vec<Reloc>| {
            blaze_ids::radix::sort_by_key(&mut relocs[..], |it| it.offset);
            // The section and the count, then each a type byte and up to three LEB128s.
            let mut contents = Vec::with_capacity(10 + relocs.len() * 16);
            section.encode(&mut contents);
            (relocs.len() as u32).encode(&mut contents);
            for reloc in relocs.iter() {
                contents.push(reloc.ty as u8);
                reloc.offset.encode(&mut contents);
                reloc.index.encode(&mut contents);
                if reloc.ty.has_addend() {
                    reloc.addend.encode(&mut contents);
                }
            }
            contents
        };
        let code_relocs = relocs(CODE_SECTION, &mut self.code_relocs);
        let data_relocs = relocs(DATA_SECTION, &mut self.data_relocs);

        let mut module = Module::new();
        module.section(&types);
        module.section(&imports);
        module.section(&functions);
        module.section(&RawSection { id: SectionId::Code as u8, data: &self.code });
        module.section(&RawSection { id: SectionId::Data as u8, data: &self.data });
        fn section<'a>(name: &'a str, data: &'a [u8]) -> CustomSection<'a> {
            CustomSection { name: name.into(), data: data.into() }
        }
        // After the data section: the reloc sections name the code and data sections by
        // their place in the module. The ones the tool conventions order come after them, in
        // that order, which the linker checks.
        const LAST: [&str; 2] = ["producers", "target_features"];
        for (name, data) in custom.iter().filter(|(name, _)| !LAST.contains(name)) {
            module.section(&section(name, data));
        }
        module.section(&section("linking", &linking));
        module.section(&section("reloc.CODE", &code_relocs));
        module.section(&section("reloc.DATA", &data_relocs));
        for last in LAST {
            for (name, data) in custom.iter().filter(|(name, _)| *name == last) {
                module.section(&section(name, data));
            }
        }
        module.finish()
    }
}

fn flags(symbol: &object::Symbol<'_, '_>) -> u32 {
    let mut flags = 0;
    if symbol.is_local() {
        flags |= BINDING_LOCAL;
    }
    if symbol.is_weak() {
        flags |= BINDING_WEAK;
    }
    if let object::SymbolFlags::Elf { st_other, .. } = symbol.flags()
        && st_other.visibility() == object::elf::STV_HIDDEN
    {
        flags |= VISIBILITY_HIDDEN;
    }
    flags
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    /// `target_features` as it was over a set of names: the bytes the bitset must write.
    fn reference(enabled: Vec<String>) -> (String, Vec<u8>) {
        let mut features = BTreeSet::new();
        for feature in enabled {
            let implied = match feature.as_str() {
                "bulk-memory" => Some("bulk-memory-opt"),
                "reference-types" => Some("call-indirect-overlong"),
                "relaxed-simd" => Some("simd128"),
                _ => None,
            };
            features.extend(implied.map(str::to_owned));
            features.insert(feature);
        }
        features.retain(|it| KNOWN_FEATURES.contains(&it.as_str()));
        let mut contents = Vec::new();
        (features.len() as u32).encode(&mut contents);
        for feature in &features {
            contents.push(b'+');
            feature.as_str().encode(&mut contents);
        }
        ("target_features".to_owned(), contents)
    }

    #[test]
    fn the_bitset_writes_the_bytes_the_set_did() {
        let names: Vec<&str> =
            KNOWN_FEATURES.iter().copied().chain(["sse2", "", "simd", "bulk-memory-opt2"]).collect();
        let mut rng = blaze_ids::rng::SplitMix64::new(0x2545_f491_4f6c_dd1d);
        for _ in 0..2_000 {
            let len = rng.index(12);
            let enabled: Vec<String> =
                (0..len).map(|_| names[rng.index(names.len())].to_owned()).collect();
            assert_eq!(target_features(enabled.clone()), reference(enabled.clone()), "{enabled:?}");
        }
        assert_eq!(target_features(Vec::new()), reference(Vec::new()));
        let all: Vec<String> = names.iter().map(|it| (*it).to_owned()).collect();
        assert_eq!(target_features(all.clone()), reference(all));
    }
}
