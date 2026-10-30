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

use cranelift_object::object::{
    self, Object, ObjectSection, ObjectSymbol, RelocationTarget, SectionIndex, SectionKind,
    SymbolIndex, SymbolKind,
};
use rustc_hash::FxHashMap;
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
    InitFuncs = 6,
    SymbolTable = 8,
}

/// The priority of the init functions whose pointers are in the section `name`, as LLVM's
/// wasm object writer reads it: `.init_array` is the default 65535, `.init_array.N` is N.
/// `None` for any other section.
pub(super) fn init_priority(name: &str) -> Option<u32> {
    match name.strip_prefix(".init_array")? {
        "" => Some(65535),
        rest => rest.strip_prefix('.')?.parse().ok(),
    }
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
    type_of: FxHashMap<WasmSignature, u32>,
    symbols: Vec<u8>,
    symbol_count: u32,
    code: Vec<u8>,
    code_relocs: Vec<Reloc>,
    data: Vec<u8>,
    data_relocs: Vec<Reloc>,
    segments: Vec<u8>,
    segment_count: u32,
    /// The `WASM_INIT_FUNCS` entries: a priority and a function symbol each.
    init_funcs: Vec<u8>,
    init_count: u32,
}

const IMPORT: &str = "blaze.import.";

/// The symbol of the function `field` that the module imports from `module`
/// (`wasm_import_module`), rather than from an `env` that some other object defines: the
/// module and the field are read back from it ([`import_of`]), so what is reused from the store
/// refers to the import as what is compiled anew does. Named for its module too, as rustc
/// mangles the name of one: two modules may have imports of the same name.
pub(super) fn import_symbol(module: &str, field: &str) -> String {
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
    signatures: &FxHashMap<String, WasmSignature>,
    custom: &[(String, Vec<u8>)],
) -> Result<Vec<u8>, String> {
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
    let referred: rustc_hash::FxHashSet<SymbolIndex> = file
        .sections()
        .flat_map(|section| section.relocations())
        .filter_map(|(_, reloc)| match reloc.target() {
            RelocationTarget::Symbol(symbol) => Some(symbol),
            _ => None,
        })
        .collect();
    let mut symbol_of: FxHashMap<SymbolIndex, (u32, bool)> = FxHashMap::default();
    let mut imported = 0u32;
    let mut functions = Vec::new();
    let mut datas = Vec::new();
    for symbol in file.symbols() {
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
        let section = file.section_by_index(index).map_err(|e| format!("wasm object: {e}"))?;
        let data = section.data().map_err(|e| format!("wasm object: {e}"))?;
        Ok::<_, String>((section, data))
    };

    let mut defined = Vec::new();
    let mut function_types = FunctionSection::new();
    let mut fixups = Vec::new();
    (functions.len() as u32).encode(&mut writer.code);
    for (at, symbol) in functions.iter().enumerate() {
        let name = symbol.name().map_err(|e| format!("wasm object: {e}"))?;
        let section = symbol.section_index().expect("a defined symbol");
        let (_, data) = section_data(section)?;
        let range = symbol.address() as usize..(symbol.address() + symbol.size()) as usize;
        let bytes = data.get(range).ok_or_else(|| format!("wasm object: `{name}` out of range"))?;
        let (function, skip) = WasmFunction::decode(bytes)?;
        function_types.function(writer.ty(&function.signature));
        (function.body.len() as u32).encode(&mut writer.code);
        let start = writer.code.len() as u32;
        writer.code.extend(&function.body);
        for &(offset, ty) in &function.type_fixups {
            let ty = writer.ty(&function.types[ty as usize]);
            fixups.push(Reloc {
                ty: RelocType::TypeIndexLeb,
                offset: start + offset,
                index: ty,
                addend: 0,
            });
        }
        for &offset in &function.stack_pointer_fixups {
            fixups.push(Reloc {
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
    // What the linker fills in has a value until then: the index in this object.
    for fixup in fixups {
        let place = &mut writer.code[fixup.offset as usize..][..INDEX_LEN];
        let value = if fixup.ty == RelocType::TypeIndexLeb { fixup.index } else { 0 };
        write_padded(place, value);
        writer.code_relocs.push(fixup);
    }

    // A pointer in `.init_array` is an init function too, which the linker calls from
    // `__wasm_call_ctors` (listed below, once every function has its symbol). The static stays
    // data as well, as rustc's object has it, since code may take its address or read it.
    let inits: Vec<_> = datas
        .iter()
        .filter_map(|symbol| {
            let section = symbol.section_index().and_then(|it| file.section_by_index(it).ok());
            Some((symbol.index(), init_priority(section?.name().ok()?)?))
        })
        .collect();

    // Each piece of data is a segment of its own, at an address of its own, so that the
    // linker drops the ones nothing refers to.
    (datas.len() as u32).encode(&mut writer.data);
    let mut address = 0u32;
    for symbol in &datas {
        let name = symbol.name().map_err(|e| format!("wasm object: {e}"))?;
        let section_index = symbol.section_index().expect("a defined symbol");
        let (section, data) = section_data(section_index)?;
        let size = symbol.size() as usize;
        let uninitialized = matches!(
            section.kind(),
            SectionKind::UninitializedData | SectionKind::UninitializedTls
        );
        let bytes = if uninitialized {
            std::borrow::Cow::Owned(vec![0; size])
        } else {
            let start = (symbol.address() - section.address()) as usize;
            let bytes = data
                .get(start..start + size)
                .ok_or_else(|| format!("wasm object: `{name}` out of range"))?;
            std::borrow::Cow::Borrowed(bytes)
        };
        // Writable data that is all zeros, with no address written into it, is `.bss` as LLVM
        // makes it: the linker leaves such segments out of the module, memory being zeros.
        let zeroed = uninitialized
            || section.kind() == SectionKind::Data
                && section.relocations().next().is_none()
                && bytes.iter().all(|&b| b == 0);
        // Each piece of data is in a section of its own (`per_data_object_section`), which is
        // aligned as the code generator asked. Its size says nothing: four bytes of a field of
        // a value aligned to 64 are aligned to 64.
        if section.address() != symbol.address() || section.size() > symbol.size() {
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
        writer.data.extend(&*bytes);
        address += size as u32;

        // The messages of the stand-ins keep their section's name, which the linker merges
        // them by into one segment of its own that the module names.
        let (_, stand_ins) = super::stand_in_section(::object::BinaryFormat::Wasm);
        let segment_name = match section.kind() {
            _ if section.name() == Ok(stand_ins) => stand_ins.to_owned(),
            _ if zeroed => format!(".bss.{name}"),
            SectionKind::ReadOnlyData | SectionKind::ReadOnlyString => format!(".rodata.{name}"),
            _ => format!(".data.{name}"),
        };
        segment_name.encode(&mut writer.segments);
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

    defined.sort_by_key(|it| (it.section.0, it.address));
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
                return Err("wasm object: relocation against no symbol".to_owned());
            };
            let target_symbol =
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
                return Err("wasm object: an address inside a function".to_owned());
            }
            let within = (offset - place.address) as u32;
            let Some(within) = within.checked_sub(place.skip) else {
                return Err("wasm object: relocation in the header of a function".to_owned());
            };
            let reloc =
                Reloc { ty, offset: place.at + within, index: symbol, addend: addend as i32 };
            if in_code { writer.code_relocs.push(reloc) } else { writer.data_relocs.push(reloc) }
        }
    }

    // Each pointer of `.init_array` is one init function, of its section's priority: a static
    // holds any number of them, as `[extern "C" fn(); 2]` two, or none, as a null one.
    for (index, priority) in inits {
        let symbol = file.symbol_by_index(index).map_err(|e| format!("wasm object: {e}"))?;
        let name = symbol.name().map_err(|e| format!("wasm object: {e}"))?;
        let section_index = symbol.section_index().expect("a defined symbol");
        let (section, _) = section_data(section_index)?;
        let range = symbol.address()..symbol.address() + symbol.size();
        for (_, reloc) in section.relocations().filter(|(offset, _)| range.contains(offset)) {
            let RelocationTarget::Symbol(target) = reloc.target() else {
                return Err("wasm object: relocation against no symbol".to_owned());
            };
            let target_symbol =
                file.symbol_by_index(target).map_err(|e| format!("wasm object: {e}"))?;
            let function = if target_symbol.kind() == SymbolKind::Section {
                target_symbol
                    .section_index()
                    .and_then(|section| containing(section, reloc.addend() as u64))
                    .filter(|it| it.is_function && it.address == reloc.addend() as u64)
                    .map(|it| it.symbol)
            } else {
                symbol_of.get(&target).filter(|(_, is_function)| *is_function).map(|&(it, _)| it)
            };
            let Some(function) = function else {
                return Err(format!(
                    "wasm object: `{name}` in `.init_array` points to no function"
                ));
            };
            priority.encode(&mut writer.init_funcs);
            function.encode(&mut writer.init_funcs);
            writer.init_count += 1;
        }
    }

    let mut sections: Vec<(&str, Vec<u8>)> = Vec::new();
    for (name, bytes) in custom {
        if matches!(name.as_str(), "linking" | "name") || name.starts_with("reloc.") {
            return Err(format!("wasm object: a custom section may not be named `{name}`"));
        }
        match sections.iter_mut().find(|(it, _)| it == name) {
            Some((_, contents)) => contents.extend(bytes),
            None => sections.push((name, bytes.clone())),
        }
    }
    Ok(writer.finish(imports, function_types, &sections))
}

/// The `target_features` section of an object compiled with `enabled` (the `target_feature`
/// values of its crates), which says what its code may use as LLVM's does: the linker joins
/// those of every object, and wasm-bindgen reads `reference-types` off the program's.
pub(super) fn target_features(enabled: impl IntoIterator<Item = String>) -> (String, Vec<u8>) {
    /// The features LLVM's wasm target knows, of the names rustc gives them too.
    const KNOWN: &[&str] = &[
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
    let mut features = std::collections::BTreeSet::new();
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
    features.retain(|it| KNOWN.contains(&it.as_str()));
    let mut contents = Vec::new();
    (features.len() as u32).encode(&mut contents);
    for feature in &features {
        contents.push(b'+');
        feature.as_str().encode(&mut contents);
    }
    ("target_features".to_owned(), contents)
}

impl Writer {
    fn ty(&mut self, signature: &WasmSignature) -> u32 {
        if let Some(&index) = self.type_of.get(signature) {
            return index;
        }
        let index = self.types.len() as u32;
        self.types.push(signature.clone());
        self.type_of.insert(signature.clone(), index);
        index
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
        custom: &[(&str, Vec<u8>)],
    ) -> Vec<u8> {
        let mut types = TypeSection::new();
        for signature in &self.types {
            let values = |types: &[super::wasm::WasmType]| {
                types.iter().map(|&it| ValType::from(it)).collect::<Vec<_>>()
            };
            types.ty().function(values(&signature.params), values(&signature.results));
        }

        let mut linking = Vec::new();
        LINKING_VERSION.encode(&mut linking);
        let mut subsection = |id: Subsection, count: u32, entries: &[u8]| {
            let mut contents = Vec::new();
            count.encode(&mut contents);
            contents.extend(entries);
            linking.push(id as u8);
            contents.encode(&mut linking);
        };
        subsection(Subsection::SymbolTable, self.symbol_count, &self.symbols);
        subsection(Subsection::SegmentInfo, self.segment_count, &self.segments);
        if self.init_count > 0 {
            subsection(Subsection::InitFuncs, self.init_count, &self.init_funcs);
        }

        // The reloc sections name the section they are for by its place in the module.
        const CODE_SECTION: u32 = 3;
        const DATA_SECTION: u32 = 4;
        let relocs = |section: u32, relocs: &mut Vec<Reloc>| {
            relocs.sort_by_key(|it| it.offset);
            let mut contents = Vec::new();
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

/// Writes `value` as a LEB128 that fills `place`.
fn write_padded(place: &mut [u8], mut value: u32) {
    let last = place.len() - 1;
    for (at, byte) in place.iter_mut().enumerate() {
        *byte = (value & 0x7f) as u8 | if at == last { 0 } else { 0x80 };
        value >>= 7;
    }
}

#[cfg(test)]
mod tests {
    use super::init_priority;

    /// `.init_array` and `.init_array.N` are init functions, as LLVM reads them; anything else
    /// that starts so is a custom section like any other.
    #[test]
    fn the_init_array_names_its_priority() {
        assert_eq!(init_priority(".init_array"), Some(65535));
        assert_eq!(init_priority(".init_array.101"), Some(101));
        assert_eq!(init_priority(".init_arrays"), None);
        assert_eq!(init_priority(".init_array.x"), None);
        assert_eq!(init_priority("blaze_joined"), None);
    }
}
