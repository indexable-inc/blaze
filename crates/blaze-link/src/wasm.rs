//! The wasm format: modules for wasm32-wasip1 and wasm32-unknown-unknown, as wasm-ld makes
//! them when rustc drives it (`-z stack-size=1048576`, `--stack-first`, `--gc-sections`,
//! `--allow-undefined`, no `--export-dynamic`), and memory64 modules for blaze's own
//! `wasm64-blaze-wasip1`, laid out the same way as wasm-ld's `-mwasm64` makes them: the memory
//! has 64-bit limits, `__stack_pointer`, `__memory_base` and an exported address are `i64`
//! globals, a data segment's offset is an `i64.const`, and an address or a function pointer is
//! an `i64.const` operand in code (a padded LEB of ten bytes) and eight bytes in data. The
//! table stays a 32-bit table of funcrefs: code wraps a slot to `i32` before `call_indirect`. A
//! memory64 module also exports `__stack_pointer` and the table (`__indirect_function_table`):
//! the seed's runner switches the one with each fiber and starts a fiber at a slot of the other.
//!
//! Code is reached by index, not address ([`Plan::indexed_code`]): the core places code (the
//! writer's `__wasm_call_ctors`, then the code pieces) in a space of its own, apart from
//! memory, and numbers the functions the imports first, then the code by its address there.
//! A function's slot in the one table of functions is its index + 1 (an element segment puts
//! every function at its slot), so a function pointer is its slot and 0 is null. Every code
//! piece is a [`blaze_codegen::wasm::Code`] header and the body; the header says the
//! function's type and where its body names a type or the `__stack_pointer` global, which the
//! writer fills.
//!
//! Memory is the stack, `[0, stack)`, then the data the core placed, then `__heap_base`. The
//! init array (the slots `__wasm_call_ctors` calls) and the custom sections (`#[link_section]`
//! of a static, an object's `producers`) are placed after the heap in the core's address
//! space, but are no part of memory. The module is written whole by [`Writer::file`]: no
//! region has a place of its own in the file.

mod code;
mod leb;
mod module;
mod object;
mod reader;
mod reloc;

use blaze_codegen::{Arch, RelocKind};

use crate::LinkError;
use crate::cow::CowBytes;
use crate::model::{
    Apply, Class, Counts, File, NativeFile, Placed, Plan, Prot, Region, SectionName, Special,
    Writer,
};
use crate::output::{Format, NativeKind, Output, OutputKind};

/// The custom section listing what a module's running code can write of the statics blaze's
/// codegen gave the link, one entry per static with any (`Stage::list_wasm_static`), in the
/// order of the statics' keys. A proc macro's host reads it to see, and carry between calls,
/// the state a macro keeps (`blaze-macro-eval`'s `wasm/statics.rs`, which parses this layout).
/// An entry, every word a little-endian `u32`:
///
/// - `__heap_base`: what an address in the module's memory at or past it is of the heap;
/// - the static's item key, 32 bytes;
/// - how many records follow, at least one;
/// - each record: where the bytes are in the module's memory, and how many. The static's own
///   bytes first when it is mutable or thread-local, then each writable allocation its value
///   points to.
///
/// Another layout is another section's name.
pub(crate) const STATICS: &str = "blaze.statics";
/// The bytes of a [`STATICS`] entry before its records, and of each record.
pub(crate) const STATICS_HEADER: usize = 40;
pub(crate) const STATICS_RECORD: usize = 8;

/// The synthetic region of the functions the linker defines: `__wasm_call_ctors`.
const FUNCTIONS: &str = "wasm.functions";
/// The 0-byte region after all data, which `__heap_base` and `__data_end` are found by.
const HEAP: &str = "wasm.heap";
const PAGE: u64 = 1 << 16;
/// wasm-ld's `-z stack-size` as rustc passes it.
const STACK: u64 = 1 << 20;
/// The alignment of the stack and of `__heap_base`, as wasm-ld's.
const STACK_ALIGN: u64 = 16;
/// The priority of an init function that names none (`.init_array`), as LLVM's.
const DEFAULT_PRIORITY: u32 = 65535;
/// The global the module defines first, and the table it has.
const STACK_POINTER: u32 = 0;
const TABLE: u32 = 0;
/// The global the module defines second, `__memory_base`: where its data starts for code
/// built position-independent, which in a module that is not is 0, as wasm-ld has it. The C
/// runtime rustc ships for wasm32-wasip1 (wasi-sdk's `crt1-reactor.o`) is built so: its
/// `_initialize` reads its flag at `__memory_base` plus an `R_WASM_MEMORY_ADDR_REL_SLEB`.
const MEMORY_BASE: u32 = 1;

pub(crate) fn writer(output: &Output) -> Result<Wasm, LinkError> {
    if output.format != Format::Wasm {
        // alloc: error path
        return Err(error(format!("the wasm writer for an output of {:?}", output.format)));
    }
    let memory64 = match output.arch {
        Arch::Wasm32 => false,
        Arch::Wasm64 => true,
        Arch::X86_64 | Arch::Aarch64 => {
            // alloc: error path
            return Err(error(format!(
                "wasm: an output for {:?}; a wasm module is wasm32 (wasm32-wasip1, \
                 wasm32-unknown-unknown) or wasm64 (wasm64-blaze-wasip1)",
                output.arch
            )));
        }
    };
    let stack = output.stack.unwrap_or(STACK);
    let stack = stack
        .checked_next_multiple_of(STACK_ALIGN)
        .filter(|&it| memory64 || it < 1 << 32)
        // alloc: error path
        .ok_or_else(|| {
            error(format!("wasm: a stack of {stack} bytes, past {:?}'s memory", output.arch))
        })?;
    Ok(Wasm { kind: output.kind.clone(), stack, memory64 }) // clone: once per link, kept by it
}

/// How the core maps a codegen relocation of wasm code (`in_code`) or data to the one the
/// writer applies: in code, a `call`'s index and an `i32.const`'s (wasm64: `i64.const`'s)
/// address or slot; in data, 4 bytes (wasm64: 8). Which kinds a module's memory takes is the
/// writer's check ([`reloc::fits_memory`]).
pub(crate) fn codegen_apply(kind: RelocKind, in_code: bool) -> Result<Apply, String> {
    let r_type = match (kind, in_code) {
        (RelocKind::WasmFunction, true) => reloc::FUNCTION_INDEX_LEB,
        (RelocKind::WasmFunction, false) => reloc::FUNCTION_INDEX_I32,
        (RelocKind::WasmTable, true) => reloc::TABLE_INDEX_SLEB,
        (RelocKind::WasmTable, false) => reloc::TABLE_INDEX_I32,
        (RelocKind::WasmMemory, true) => reloc::MEMORY_ADDR_SLEB,
        (RelocKind::WasmMemory, false) => reloc::MEMORY_ADDR_I32,
        (RelocKind::WasmTable64, true) => reloc::TABLE_INDEX_SLEB64,
        (RelocKind::WasmTable64, false) => reloc::TABLE_INDEX_I64,
        (RelocKind::WasmMemory64, true) => reloc::MEMORY_ADDR_SLEB64,
        (RelocKind::WasmMemory64, false) => reloc::MEMORY_ADDR_I64,
        (kind, _) => {
            // alloc: error path
            return Err(format!(
                "wasm: a {kind:?} relocation, where wasm code has only WasmFunction, WasmTable, \
                 WasmMemory, WasmTable64 and WasmMemory64"
            ));
        }
    };
    Ok(Apply::Native(r_type))
}

fn error(what: String) -> LinkError {
    LinkError { function: None, what, refusal: None }
}

pub(crate) struct Wasm {
    kind: OutputKind,
    /// The stack's size, a multiple of 16: data starts there.
    stack: u64,
    /// A memory64 module (wasm64): 64-bit addresses, `i64` address globals and offsets.
    memory64: bool,
}

/// A data segment of this name, which `__start_`/`__stop_` name when it is a C identifier.
fn named_data(name: &str) -> Class {
    Class::Named { name: SectionName::new(name), write: true, exec: false }
}

/// A custom section of the module: placed after the heap, in no memory.
fn custom(name: &str) -> Class {
    Class::Named { name: SectionName::new(name), write: false, exec: false }
}

fn is_code(class: &Class) -> bool {
    match class {
        Class::Plt | Class::Text | Class::Patch | Class::Stubs => true,
        Class::Synthetic(name) => *name == FUNCTIONS,
        _ => false,
    }
}

fn is_custom(class: &Class) -> bool {
    matches!(class, Class::Named { write: false, .. })
}

/// Whether a region of this class is part of the module's memory.
pub(crate) fn in_memory(class: &Class) -> bool {
    !is_code(class) && !is_custom(class) && *class != Class::InitArray
}

/// Where a class goes: the code, `__wasm_call_ctors` first as wasm-ld numbers it, then
/// memory, the heap's start after all data, then what is in no memory.
fn place(class: &Class) -> u8 {
    match class {
        Class::Plt => 0,
        Class::Synthetic(name) if *name == FUNCTIONS => 1,
        Class::Text => 2,
        Class::Patch => 3,
        Class::Stubs => 4,
        Class::Rodata => 10,
        Class::RelRo => 11,
        Class::Data => 12,
        Class::Named { write: true, .. } => 13,
        Class::Table => 14,
        Class::Got => 15,
        Class::EhFrame => 16,
        Class::ThreadVars => 17,
        Class::Tdata => 18,
        Class::Tbss => 19,
        Class::FiniArray => 20,
        Class::PatchData => 21,
        Class::Bss => 22,
        Class::Synthetic(name) if *name == HEAP => 24,
        Class::Synthetic(_) => 23,
        Class::InitArray => 30,
        Class::Named { write: false, .. } => 31,
        // Never in a module: wasm has no `.preinit_array`, and a dev run of wasm is refused
        // (`start_hook`).
        Class::PreinitArray | Class::Reserve(_) => 32,
    }
}

/// The end of the data: where the last region of memory before the heap ends.
fn data_end(regions: &[Region]) -> u64 {
    let heap = regions.iter().find(|it| it.class == Class::Synthetic(HEAP)).map_or(0, |it| it.addr);
    regions
        .iter()
        .filter(|it| in_memory(&it.class) && it.class != Class::Synthetic(HEAP))
        .map(|it| it.addr + it.size)
        .max()
        .map_or(heap, |end| end.min(heap))
}

/// `__heap_base`: the heap's region, aligned as wasm-ld aligns it.
fn heap_base(regions: &[Region]) -> u64 {
    let heap = regions.iter().find(|it| it.class == Class::Synthetic(HEAP)).map_or(0, |it| it.addr);
    heap.next_multiple_of(STACK_ALIGN)
}

/// `__heap_end`: the end of the module's memory as it starts, the pages up to the heap's
/// start (at least one), which is where wasm-ld puts it with no `--initial-memory`; wasi-libc's
/// `malloc` takes the memory from `__heap_base` to it before it grows the memory. The module's
/// memory section has as many pages ([`module`]).
fn heap_end(regions: &[Region]) -> u64 {
    initial_pages(regions) * PAGE
}

/// How many pages the module's memory has as it starts: to the heap's start, at least one.
fn initial_pages(regions: &[Region]) -> u64 {
    heap_base(regions).div_ceil(PAGE).max(1)
}

/// The name of a C identifier, of which wasm-ld defines `__start_` and `__stop_` symbols.
fn is_c_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|it| it.is_ascii_alphabetic() || it == '_')
        && chars.all(|it| it.is_ascii_alphanumeric() || it == '_')
}

/// The priority of an init function in a section of this name: `.init_array.N`, or the
/// default for `.init_array`.
fn init_priority(name: &str) -> Result<Option<u32>, String> {
    if name == ".init_array" {
        return Ok(Some(DEFAULT_PRIORITY));
    }
    match name.strip_prefix(".init_array.") {
        Some(priority) => match priority.parse::<u32>() {
            Ok(priority) if priority <= DEFAULT_PRIORITY => Ok(Some(priority)),
            // alloc: error path
            _ => Err(format!("`{name}`: an init priority must be a number up to 65535")),
        },
        None => Ok(None),
    }
}

impl Writer for Wasm {
    fn format(&self) -> Format {
        Format::Wasm
    }

    fn arch(&self) -> Arch {
        if self.memory64 { Arch::Wasm64 } else { Arch::Wasm32 }
    }

    fn plan(&self) -> Plan {
        Plan {
            base: 0,
            headers: self.stack,
            // Memory has no protections: data follows the stack at its own alignment, as
            // wasm-ld places it.
            page: STACK_ALIGN,
            indexed_code: true,
            got_by_library: false,
            tls_descriptors: false,
        }
    }

    fn order(&self, classes: &mut Vec<Class>) {
        fn key(class: &Class) -> (u8, &str) {
            let name = match class {
                Class::Named { name, .. } => name.as_str(),
                _ => "",
            };
            (place(class), name)
        }
        classes.sort_by(|a, b| key(a).cmp(&key(b)));
        classes.dedup();
    }

    fn prot(&self, class: &Class) -> Prot {
        if is_code(class) { Prot::RX } else { Prot::RW }
    }

    /// rustc's rule for `#[link_section]` on wasm: `.init_array*` holds init functions, any
    /// other name is a custom section of the module.
    fn section(
        &self,
        name: &str,
        _write: bool,
        exec: bool,
        _tls: bool,
    ) -> Result<(Class, u32), String> {
        if exec {
            // alloc: error path
            return Err(format!("`{name}` is a section of code: wasm code is functions"));
        }
        if let Some(priority) = init_priority(name)? {
            return Ok((Class::InitArray, priority));
        }
        if name.starts_with(".fini_array") {
            // alloc: error path
            return Err(format!("`{name}`: wasm has no fini functions (LLVM refuses them too)"));
        }
        Ok((custom(name), 0))
    }

    fn start_hook(&self) -> Result<(Class, u32), String> {
        // alloc: error path
        Err("a wasm module has no code memory to start in: a dev run of wasm links with no `dev_run`".to_owned())
    }

    fn special(&self, name: &str) -> Option<Special> {
        Some(match name {
            "__stack_pointer" => Special::Absolute(STACK_POINTER.into()),
            "__indirect_function_table" => Special::Absolute(TABLE.into()),
            "__memory_base" => Special::Absolute(MEMORY_BASE.into()),
            "__heap_base" => Special::Placed(heap_base),
            "__heap_end" => Special::Placed(heap_end),
            // The address past the first page, which wasi-libc reads its page size from.
            "__wasm_first_page_end" => Special::Absolute(PAGE),
            "__data_end" => Special::Placed(data_end),
            "__dso_handle" => Special::Absolute(self.stack),
            "__global_base" | "__stack_low" => Special::Absolute(0),
            "__stack_high" => Special::Absolute(self.stack),
            "__wasm_call_ctors" => Special::Start(Class::Synthetic(FUNCTIONS)),
            "__blaze_live" => Special::LiveDescriptor,
            _ => {
                if let Some(section) = name.strip_prefix("__start_")
                    && is_c_identifier(section)
                {
                    return Some(Special::Start(named_data(section)));
                }
                if let Some(section) = name.strip_prefix("__stop_")
                    && is_c_identifier(section)
                {
                    return Some(Special::Stop(named_data(section)));
                }
                return None;
            }
        })
    }

    fn read(&self, name: &str, bytes: &[u8], kind: &NativeKind) -> Result<NativeFile, String> {
        object::read(name, bytes, kind, self.memory64)
    }

    /// `__wasm_call_ctors` takes one unit of the code space, the heap's start none.
    fn synthetic(&self, _counts: &Counts) -> Vec<(Class, u64)> {
        // alloc: once per link, the `Vec` `Writer::synthetic` returns
        vec![(Class::Synthetic(FUNCTIONS), 1), (Class::Synthetic(HEAP), 0)]
    }

    fn apply(
        &self,
        r_type: u32,
        bytes: &mut [u8],
        _p: u64,
        s: u64,
        addend: i64,
    ) -> Result<(), String> {
        reloc::fits_memory(r_type, self.memory64)?;
        reloc::apply(r_type, bytes, s, addend)
    }

    /// `__wasm_call_ctors` is written with the module ([`Writer::file`]), from the init array
    /// as it is then: its region holds one unit, which the code space counts it by.
    fn synthesize(&self, placed: &Placed<'_>) -> Result<Vec<(Class, CowBytes)>, LinkError> {
        // alloc: error path
        module::init_functions(placed, self.memory64).map_err(|e| error(format!("wasm: {e}")))?;
        // wasm keeps no table across links: each region's bytes are wrapped once.
        // alloc: once per link, the regions' bytes `Writer::synthesize` returns, and their `Arc`s
        let (functions, heap) = (CowBytes::from_vec(vec![0]), CowBytes::from_vec(Vec::new()));
        // alloc: once per link, the list `Writer::synthesize` returns
        Ok(vec![(Class::Synthetic(FUNCTIONS), functions), (Class::Synthetic(HEAP), heap)])
    }

    fn offset(&self, _placed: &Placed<'_>, _region: &Region) -> Option<u64> {
        None
    }

    fn file(&self, placed: &Placed<'_>) -> Result<File, LinkError> {
        // alloc: error path
        let module = module::write(self, placed).map_err(|e| error(format!("wasm: {e}")))?;
        // alloc: once per link, the one part of the `File` (the module, written whole, and its
        // `Arc`: wasm keeps no table across links)
        // alloc: none for `debug`: an empty `Vec` holds no buffer
        Ok(File {
            len: module.len() as u64,
            parts: vec![(0, CowBytes::from_vec(module))],
            debug: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::Debug;

    fn output(arch: Arch, kind: OutputKind, stack: Option<u64>) -> Output {
        Output {
            format: Format::Wasm,
            arch,
            kind,
            interpreter: None,
            platform: None,
            incremental: false,
            debug: Debug::None,
            live: None,
            tags: 0,
            stack,
            windows_subsystem: false,
            dev_run: None,
            exec_log: false,
            sanitize: blaze_codegen::settings::Sanitize::default(),
            rpaths: Vec::new(),
            link: crate::LinkArgs::default(),
            triple: "wasm32-unknown-unknown".into(),
        }
    }

    fn command() -> OutputKind {
        OutputKind::Executable { entry: "_start".to_owned() }
    }

    fn region(class: Class, addr: u64, size: u64) -> Region {
        Region { class, addr, capacity: size, size, prot: Prot::RW, align: 8 }
    }

    #[test]
    fn only_wasm() {
        let error = writer(&output(Arch::X86_64, command(), None)).err().unwrap();
        assert!(error.what.contains("X86_64"), "{}", error.what);
        assert!(writer(&output(Arch::Aarch64, command(), None)).is_err());
        let mut elf = output(Arch::Wasm32, command(), None);
        elf.format = Format::Elf;
        assert!(writer(&elf).is_err());
        assert!(writer(&output(Arch::Wasm32, command(), Some(1 << 33))).is_err());
        // A memory64 module's stack may pass 4 GiB.
        let wasm64 = writer(&output(Arch::Wasm64, command(), Some(1 << 33))).unwrap();
        assert_eq!((wasm64.arch(), wasm64.plan().headers), (Arch::Wasm64, 1 << 33));
        assert!(writer(&output(Arch::Wasm64, command(), Some(u64::MAX))).is_err());
    }

    #[test]
    fn plan_is_stack_first() {
        let wasm = writer(&output(Arch::Wasm32, command(), None)).unwrap();
        let plan = wasm.plan();
        assert_eq!((plan.base, plan.headers, plan.page), (0, 1 << 20, 16));
        assert!(plan.indexed_code && !plan.got_by_library && !plan.tls_descriptors);
        let shared = OutputKind::Shared { name: "lib.wasm".to_owned(), exports: Vec::new() };
        let wasm = writer(&output(Arch::Wasm32, shared, Some(1000))).unwrap();
        assert_eq!(wasm.plan().headers, 1008);
        assert_eq!(wasm.format(), Format::Wasm);
        assert_eq!(wasm.arch(), Arch::Wasm32);
    }

    #[test]
    fn order_puts_code_first_and_the_heap_after_data() {
        let wasm = writer(&output(Arch::Wasm32, command(), None)).unwrap();
        let mut classes = vec![
            custom("producers"),
            Class::Bss,
            Class::Synthetic(HEAP),
            Class::Text,
            named_data("my_items"),
            Class::Rodata,
            Class::Synthetic(FUNCTIONS),
            Class::RelRo,
            Class::Tdata,
            Class::Data,
            Class::InitArray,
            Class::PatchData,
            custom("a_custom"),
        ];
        wasm.order(&mut classes);
        assert_eq!(
            classes,
            [
                Class::Synthetic(FUNCTIONS),
                Class::Text,
                Class::Rodata,
                Class::RelRo,
                Class::Data,
                named_data("my_items"),
                Class::Tdata,
                Class::PatchData,
                Class::Bss,
                Class::Synthetic(HEAP),
                Class::InitArray,
                custom("a_custom"),
                custom("producers"),
            ]
        );
        assert_eq!(wasm.prot(&Class::Text), Prot::RX);
        assert_eq!(wasm.prot(&Class::Synthetic(FUNCTIONS)), Prot::RX);
        assert_eq!(wasm.prot(&Class::Rodata), Prot::RW);
        assert_eq!(wasm.prot(&Class::Synthetic(HEAP)), Prot::RW);
        assert!(in_memory(&Class::Tbss) && in_memory(&Class::RelRo));
        assert!(!in_memory(&Class::InitArray) && !in_memory(&custom("producers")));
        assert!(!in_memory(&Class::Text) && !in_memory(&Class::Synthetic(FUNCTIONS)));
    }

    #[test]
    fn sections_as_rustc_makes_them() {
        let wasm = writer(&output(Arch::Wasm32, command(), None)).unwrap();
        assert_eq!(wasm.section(".init_array", true, false, false), Ok((Class::InitArray, 65535)));
        assert_eq!(
            wasm.section(".init_array.101", true, false, false),
            Ok((Class::InitArray, 101))
        );
        assert!(wasm.section(".init_array.x", true, false, false).is_err());
        assert!(wasm.section(".init_array.70000", true, false, false).is_err());
        assert!(wasm.section(".fini_array", true, false, false).is_err());
        assert!(wasm.section(".text.f", false, true, false).is_err());
        assert_eq!(
            wasm.section("__wasm_bindgen_unstable", false, false, false),
            Ok((custom("__wasm_bindgen_unstable"), 0))
        );
    }

    #[test]
    fn specials() {
        let wasm = writer(&output(Arch::Wasm32, command(), None)).unwrap();
        assert_eq!(wasm.special("__stack_pointer"), Some(Special::Absolute(0)));
        assert_eq!(wasm.special("__indirect_function_table"), Some(Special::Absolute(0)));
        assert_eq!(wasm.special("__memory_base"), Some(Special::Absolute(1)));
        assert_eq!(wasm.special("__wasm_first_page_end"), Some(Special::Absolute(1 << 16)));
        assert_eq!(wasm.special("__dso_handle"), Some(Special::Absolute(1 << 20)));
        assert_eq!(wasm.special("__stack_high"), Some(Special::Absolute(1 << 20)));
        assert_eq!(
            wasm.special("__wasm_call_ctors"),
            Some(Special::Start(Class::Synthetic(FUNCTIONS)))
        );
        assert_eq!(wasm.special("__blaze_live"), Some(Special::LiveDescriptor));
        assert_eq!(wasm.special("__start_my_items"), Some(Special::Start(named_data("my_items"))));
        assert_eq!(wasm.special("__stop_my_items"), Some(Special::Stop(named_data("my_items"))));
        assert_eq!(wasm.special("__start_.data"), None);
        assert_eq!(wasm.special("__tls_base"), None);
        assert_eq!(wasm.special("main"), None);

        // Data to 0x10_0023, the heap's region at the next 8; the custom section and the init
        // array after it are no part of memory, nor is code.
        let regions = [
            region(Class::Synthetic(FUNCTIONS), 0, 1),
            region(Class::Text, 1, 300),
            region(Class::Rodata, 0x10_0000, 0x20),
            region(Class::Bss, 0x10_0020, 3),
            region(Class::Synthetic(HEAP), 0x10_0028, 0),
            region(Class::InitArray, 0x10_0028, 8),
            region(custom("producers"), 0x10_0030, 40),
        ];
        let at = |name: &str| match wasm.special(name) {
            Some(Special::Placed(at)) => at(&regions),
            other => panic!("`{name}` is {other:?}"),
        };
        assert_eq!(at("__data_end"), 0x10_0023);
        assert_eq!(at("__heap_base"), 0x10_0030);
        let nothing = [region(Class::Synthetic(HEAP), 0x10_0000, 0)];
        assert_eq!((data_end(&nothing), heap_base(&nothing)), (0x10_0000, 0x10_0000));
    }

    #[test]
    fn synthetic_regions() {
        let wasm = writer(&output(Arch::Wasm32, command(), None)).unwrap();
        assert_eq!(
            wasm.synthetic(&Counts::default()),
            [(Class::Synthetic(FUNCTIONS), 1), (Class::Synthetic(HEAP), 0)]
        );
    }

    #[test]
    fn codegen_relocations() {
        let native = |r_type| Ok(Apply::Native(r_type));
        assert_eq!(codegen_apply(RelocKind::WasmFunction, true), native(reloc::FUNCTION_INDEX_LEB));
        assert_eq!(
            codegen_apply(RelocKind::WasmFunction, false),
            native(reloc::FUNCTION_INDEX_I32)
        );
        assert_eq!(codegen_apply(RelocKind::WasmTable, true), native(reloc::TABLE_INDEX_SLEB));
        assert_eq!(codegen_apply(RelocKind::WasmTable, false), native(reloc::TABLE_INDEX_I32));
        assert_eq!(codegen_apply(RelocKind::WasmMemory, true), native(reloc::MEMORY_ADDR_SLEB));
        assert_eq!(codegen_apply(RelocKind::WasmMemory, false), native(reloc::MEMORY_ADDR_I32));
        assert_eq!(codegen_apply(RelocKind::WasmTable64, true), native(reloc::TABLE_INDEX_SLEB64));
        assert_eq!(codegen_apply(RelocKind::WasmTable64, false), native(reloc::TABLE_INDEX_I64));
        assert_eq!(codegen_apply(RelocKind::WasmMemory64, true), native(reloc::MEMORY_ADDR_SLEB64));
        assert_eq!(codegen_apply(RelocKind::WasmMemory64, false), native(reloc::MEMORY_ADDR_I64));
        assert!(codegen_apply(RelocKind::Abs64, false).is_err());
        assert!(codegen_apply(RelocKind::TlsOffset, true).is_err());
    }

    #[test]
    fn apply_goes_to_the_relocations() {
        let wasm = writer(&output(Arch::Wasm32, command(), None)).unwrap();
        let mut bytes = [0; 5];
        wasm.apply(reloc::FUNCTION_INDEX_LEB, &mut bytes, 0, 3, 0).unwrap();
        assert_eq!(bytes, leb::padded_uleb(3));
        assert!(wasm.apply(77, &mut bytes, 0, 3, 0).is_err());
        // Each module takes only its memory's width of address.
        let mut wide = [0; 10];
        assert!(wasm.apply(reloc::MEMORY_ADDR_SLEB64, &mut wide, 0, 3, 0).is_err());
        let wasm64 = writer(&output(Arch::Wasm64, command(), None)).unwrap();
        wasm64.apply(reloc::MEMORY_ADDR_SLEB64, &mut wide, 0, 3, 0).unwrap();
        assert_eq!(wide, leb::padded_sleb64(3));
        assert!(wasm64.apply(reloc::MEMORY_ADDR_SLEB, &mut bytes, 0, 3, 0).is_err());
        wasm64.apply(reloc::FUNCTION_INDEX_LEB, &mut bytes, 0, 3, 0).unwrap();
    }
}
