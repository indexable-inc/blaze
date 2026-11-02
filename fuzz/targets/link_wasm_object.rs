//! wasm relocatable objects (a module with a `linking` section and `reloc.*` sections, as
//! LLVM writes them), read as the linker reads one on its link line:
//! `blaze_link::fuzz_read(Format::Wasm, Arch::Wasm32, NativeKind::Object, bytes)`.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - the read returns, accepting or refusing, and never panics (a read past the input is a
//!   slice index out of range in the plain test, and ASan's report in the fuzz build);
//! - the largest single allocation of a read is at most `blaze_fuzz::allowed(len)` (counts
//!   from LEB128s are bounded by the bytes left before anything is sized by them);
//! - the bytes at an address 8 does not divide read as they do 8-aligned.
//!
//! A module's sections run to the end of the input, so bytes after it are another section:
//! no invariant about padding holds here.

#![forbid(unsafe_code)]

use std::time::Duration;

use blaze_codegen::Arch;
use blaze_fuzzer::Target;
use blaze_link::{Format, NativeKind};

const TARGET: Target = Target {
    name: "link_wasm_object",
    check,
    seeds,
    dictionary: &[
        b"\0asm\x01\0\0\0",
        b"\x07linking\x02",
        b"\x0areloc.CODE",
        b"\x0areloc.DATA",
        b"\x09producers",
        b"\x0b.debug_info",
        b"\x08target_features",
        b"\x80\x80\x80\x80\x00",
        b"\xff\xff\xff\xff\x0f",
        b"\x60",
        b"\x7f",
        b"\x70",
        b"\x0b",
        b"\x41",
        b"\x10",
        b"\x11",
        b"\x23",
        b"env",
        b"__linear_memory",
        b"__stack_pointer",
        b"__indirect_function_table",
        b".rodata.",
        b".data.",
        b".bss.",
        b".tdata.",
        b".init_array",
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(600),
};

const OBJECT: NativeKind = NativeKind::Object;

fn check(data: &[u8]) {
    let (aligned, at) = copy_at(data, 0);
    let aligned = &aligned[at..][..data.len()];
    let (shifted, at) = copy_at(data, 1);
    let shifted = &shifted[at..][..data.len()];
    let bound = blaze_fuzz::allowed(data.len());
    let (largest, read) =
        blaze_fuzz::largest_allocation(|| blaze_link::fuzz_read(Format::Wasm, Arch::Wasm32, OBJECT, aligned));
    assert!(
        largest <= bound,
        "allocation bound: a read of {} bytes asked for {largest} bytes at once, past {bound}",
        data.len()
    );
    assert_eq!(
        blaze_link::fuzz_read(Format::Wasm, Arch::Wasm32, OBJECT, shifted),
        read,
        "alignment: a read of the bytes one past an 8-aligned address answers otherwise"
    );
}

/// `data` copied to `shift` past an 8-aligned address, and where it starts in the copy.
fn copy_at(data: &[u8], shift: usize) -> (Vec<u8>, usize) {
    let mut buffer = vec![0xff; data.len() + 16];
    let start = (8 - buffer.as_ptr() as usize % 8) % 8 + shift;
    buffer[start..start + data.len()].copy_from_slice(data);
    (buffer, start)
}

fn seeds() -> Vec<Vec<u8>> {
    named().into_iter().map(|(_, bytes)| bytes).collect()
}

fn named() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("an object of every kind of thing the reader takes", object(&code_relocs())),
        ("an object of one function", minimal("f")),
    ]
}

const MAGIC: &[u8; 8] = b"\0asm\x01\0\0\0";

const SEGMENT_INFO: u8 = 5;
const INIT_FUNCS: u8 = 6;
const COMDAT_INFO: u8 = 7;
const SYMBOL_TABLE: u8 = 8;
const LINKING_VERSION: u64 = 2;

const SYM_FUNCTION: u8 = 0;
const SYM_DATA: u8 = 1;
const SYM_GLOBAL: u8 = 2;
const SYM_SECTION: u8 = 3;
const SYM_TABLE: u8 = 5;

const BINDING_LOCAL: u8 = 0x2;
const VISIBILITY_HIDDEN: u8 = 0x4;
const UNDEFINED: u8 = 0x10;
const EXPORTED: u8 = 0x20;
const EXPLICIT_NAME: u8 = 0x40;

const COMDAT_DATA: u8 = 0;
const COMDAT_FUNCTION: u8 = 1;

/// The `__stack_pointer` symbol's index.
const SP: u32 = 3;

fn uleb(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn sleb(out: &mut Vec<u8>, mut value: i64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if (value == 0 && byte & 0x40 == 0) || (value == -1 && byte & 0x40 != 0) {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn name(out: &mut Vec<u8>, name: &str) {
    uleb(out, name.len() as u64);
    out.extend(name.as_bytes());
}

fn section(out: &mut Vec<u8>, id: u8, contents: &[u8]) {
    out.push(id);
    uleb(out, contents.len() as u64);
    out.extend(contents);
}

fn custom(out: &mut Vec<u8>, custom: &str, payload: &[u8]) {
    let mut contents = Vec::new();
    name(&mut contents, custom);
    contents.extend(payload);
    section(out, 0, &contents);
}

fn vec_of(count: usize, entries: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    uleb(&mut out, count as u64);
    out.extend(entries);
    out
}

/// The relocation types with an addend (`reloc::has_addend`).
fn has_addend(r_type: u8) -> bool {
    matches!(r_type, 3 | 4 | 5 | 8 | 9 | 11 | 14 | 15 | 16 | 17 | 21 | 22 | 23 | 25)
}

/// Relocations: type, offset in the section, symbol or type, addend.
fn reloc_section(out: &mut Vec<u8>, section_name: &str, section_index: u32, relocs: &[(u8, u32, u32, i64)]) {
    let mut contents = Vec::new();
    uleb(&mut contents, section_index.into());
    uleb(&mut contents, relocs.len() as u64);
    for &(r_type, offset, index, addend) in relocs {
        contents.push(r_type);
        uleb(&mut contents, offset.into());
        uleb(&mut contents, index.into());
        if has_addend(r_type) {
            sleb(&mut contents, addend);
        }
    }
    custom(out, section_name, &contents);
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

/// `_start`'s relocations, at their places in the code section (the body starts at 2).
fn code_relocs() -> Vec<(u8, u32, u32, i64)> {
    vec![(7, 4, SP, 0), (4, 11, 4, 2), (1, 17, 1, 0), (6, 23, 2, 0), (20, 28, 8, 0), (0, 34, 2, 0)]
}

/// An object as clang writes one: imports of memory, the stack pointer, a WASI function and
/// the table; two functions (one exported, one an init function in a COMDAT); four data
/// segments; `producers` and `.debug_info` custom sections; a section symbol; relocations of
/// the code, the data and the debug info.
fn object(code_relocs: &[(u8, u32, u32, i64)]) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
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
    uleb(&mut linking, LINKING_VERSION);
    let mut symbols = Vec::new();
    let mut count = 0;
    let mut sym = |bytes: &[u8]| {
        symbols.extend(bytes);
        count += 1;
    };
    let mut entry = vec![SYM_FUNCTION, EXPORTED, 1];
    name(&mut entry, "_start");
    sym(&entry);
    let mut entry = vec![SYM_FUNCTION, BINDING_LOCAL, 2];
    name(&mut entry, "init");
    sym(&entry);
    let mut entry = vec![SYM_FUNCTION, UNDEFINED | EXPLICIT_NAME, 0];
    name(&mut entry, "__imported_wasi_snapshot_preview1_fd_write");
    sym(&entry);
    sym(&[SYM_GLOBAL, UNDEFINED, 0]);
    let mut entry = vec![SYM_DATA, 0];
    name(&mut entry, "msg");
    entry.extend([0, 0, 4]);
    sym(&entry);
    let mut entry = vec![SYM_DATA, VISIBILITY_HIDDEN];
    name(&mut entry, "buf");
    entry.extend([1, 0, 8]);
    sym(&entry);
    let mut entry = vec![SYM_DATA, 0];
    name(&mut entry, "ptr");
    entry.extend([2, 0, 4]);
    sym(&entry);
    let mut entry = vec![SYM_DATA, UNDEFINED];
    name(&mut entry, "extern_data");
    sym(&entry);
    sym(&[SYM_TABLE, UNDEFINED, 0]);
    sym(&[SYM_SECTION, BINDING_LOCAL, 7]);
    // Flags 0x80 (NO_STRIP), a two-byte LEB128.
    let mut entry = vec![SYM_DATA, 0x80, 0x01];
    name(&mut entry, "items");
    entry.extend([3, 0, 4]);
    sym(&entry);
    let mut segments = Vec::new();
    for (segment, align) in [(".rodata.msg", 0), (".bss.buf", 3), (".data.ptr", 2), ("my_items", 2)] {
        name(&mut segments, segment);
        segments.extend([align, 0]);
    }
    let mut subsection = |id: u8, contents: &[u8]| {
        linking.push(id);
        uleb(&mut linking, contents.len() as u64);
        linking.extend(contents);
    };
    subsection(SEGMENT_INFO, &vec_of(4, &segments));
    subsection(INIT_FUNCS, &vec_of(1, &[101, 1]));
    let mut comdat = Vec::new();
    name(&mut comdat, "grp");
    comdat.extend([0, 2, COMDAT_FUNCTION, 2, COMDAT_DATA, 2]);
    subsection(COMDAT_INFO, &vec_of(1, &comdat));
    subsection(SYMBOL_TABLE, &vec_of(count, &symbols));
    custom(&mut out, "linking", &linking);

    reloc_section(&mut out, "reloc.CODE", 4, code_relocs);
    reloc_section(&mut out, "reloc.DATA", 5, &[(2, 28, 1, 0)]);
    // `.debug_info`'s contents start with its name: 12 bytes.
    reloc_section(&mut out, "reloc..debug_info", 7, &[(9, 12, 9, 0)]);
    out
}

/// An object of one function `() -> ()` named `function`, and nothing else.
fn minimal(function: &str) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    section(&mut out, 1, &vec_of(1, &[0x60, 0, 0]));
    section(&mut out, 3, &vec_of(1, &[0]));
    section(&mut out, 10, &vec_of(1, &[2, 0x00, 0x0b]));
    let mut entry = vec![SYM_FUNCTION, 0, 0];
    name(&mut entry, function);
    let mut linking = Vec::new();
    uleb(&mut linking, LINKING_VERSION);
    let table = vec_of(1, &entry);
    linking.push(SYMBOL_TABLE);
    uleb(&mut linking, table.len() as u64);
    linking.extend(table);
    custom(&mut out, "linking", &linking);
    out
}

#[cfg_attr(test, allow(dead_code))]
fn main() -> std::process::ExitCode {
    blaze_fuzzer::main(&TARGET)
}

#[cfg(test)]
mod tests {
    use blaze_codegen::Arch;
    use blaze_link::Format;

    use super::{OBJECT, named};

    #[test]
    fn seeds() {
        blaze_fuzzer::seeds(&super::TARGET);
    }

    #[test]
    fn seeds_read() {
        for (what, bytes) in named() {
            assert!(blaze_link::fuzz_read(Format::Wasm, Arch::Wasm32, OBJECT, &bytes), "seed `{what}` is refused");
        }
    }
}
