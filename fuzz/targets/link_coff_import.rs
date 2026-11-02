//! Import libraries (`.lib` archives of short import members, or of long ones: an import
//! descriptor and per symbol an object with `.idata$5` and `.idata$6`), read as the linker
//! reads a DLL's library: `blaze_link::fuzz_read(Format::Pe, Arch::X86_64, NativeKind::Shared {
//! as_needed }, bytes)`. Every member is read.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - the read returns, accepting or refusing, and never panics (a read past the input is a
//!   slice index out of range in the plain test, and ASan's report in the fuzz build; an
//!   offset that overflows panics in the plain test too);
//! - the largest single allocation of a read is at most `blaze_fuzz::allowed(len)`;
//! - the bytes at an address 8 does not divide read as they do 8-aligned;
//! - `as_needed` changes nothing the read accepts.

#![forbid(unsafe_code)]

use std::time::Duration;

use blaze_codegen::Arch;
use blaze_fuzzer::Target;
use blaze_link::{Format, NativeKind};
use object::write::{Object as Builder, Relocation, Symbol, SymbolSection};
use object::{Architecture, BinaryFormat, Endianness, RelocationFlags, SectionKind, SymbolFlags, SymbolKind, SymbolScope, pe};

const TARGET: Target = Target {
    name: "link_coff_import",
    check,
    seeds,
    dictionary: &[
        b"!<arch>\n",
        b"`\n",
        b"/               ",
        b"//              ",
        b"MZ",
        b"__IMPORT_DESCRIPTOR_",
        b"__NULL_IMPORT_DESCRIPTOR",
        b"_NULL_THUNK_DATA",
        b"__imp_",
        b".idata$2",
        b".idata$4",
        b".idata$5",
        b".idata$6",
        b"\x00\x00\xff\xff\x00\x00\x64\x86",
        b"\x64\x86",
        b"\x03\x00",
        b"\x00\x00\x00\x00\x00\x00\x00\x80",
        b".dll\0",
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(600),
};

const SHARED: NativeKind = NativeKind::Shared { as_needed: false };
const AS_NEEDED: NativeKind = NativeKind::Shared { as_needed: true };

/// The read compared with the first (shifted, as_needed), one a run
/// (`blaze_fuzzer::case`); the plain test takes every one of each seed.
const CASES: usize = 2;

fn check(data: &[u8]) {
    check_case(data, blaze_fuzzer::case(data, CASES));
}

fn check_case(data: &[u8], case: usize) {
    let (aligned, at) = copy_at(data, 0);
    let aligned = &aligned[at..][..data.len()];
    let bound = blaze_fuzz::allowed(data.len());
    let (largest, read) =
        blaze_fuzz::largest_allocation(|| blaze_link::fuzz_read(Format::Pe, Arch::X86_64, SHARED, aligned));
    assert!(
        largest <= bound,
        "allocation bound: a read of {} bytes asked for {largest} bytes at once, past {bound}",
        data.len()
    );
    match case {
        0 => {
            let (shifted, at) = copy_at(data, 1);
            let shifted = &shifted[at..][..data.len()];
            assert_eq!(
                blaze_link::fuzz_read(Format::Pe, Arch::X86_64, SHARED, shifted),
                read,
                "alignment: a read of the bytes one past an 8-aligned address answers otherwise"
            );
        }
        _ => {
            assert_eq!(
                blaze_link::fuzz_read(Format::Pe, Arch::X86_64, AS_NEEDED, aligned),
                read,
                "as needed: a read with `as_needed` answers otherwise"
            );
        }
    }
}

/// `data` copied to `shift` past an 8-aligned address, and where it starts in the copy.
fn copy_at(data: &[u8], shift: usize) -> (Vec<u8>, usize) {
    let mut buffer = vec![0xff; data.len() + 16];
    let start = (8 - buffer.as_ptr() as usize % 8) % 8 + shift;
    buffer[start..start + data.len()].copy_from_slice(data);
    (buffer, start)
}

fn seeds() -> Vec<Vec<u8>> {
    let mut seeds: Vec<Vec<u8>> = named().into_iter().map(|(_, bytes)| bytes).collect();
    seeds.push(thunk_past_usize());
    seeds
}

fn named() -> Vec<(&'static str, Vec<u8>)> {
    let code = pe::IMPORT_OBJECT_CODE.0 | (pe::IMPORT_OBJECT_NAME.0 << 2);
    let data = pe::IMPORT_OBJECT_DATA.0 | (pe::IMPORT_OBJECT_NAME_NO_PREFIX.0 << 2);
    let ordinal = pe::IMPORT_OBJECT_CODE.0 | (pe::IMPORT_OBJECT_ORDINAL.0 << 2);
    let short = [
        ("foo.dll", short_import(code, 3, &["f", "foo.dll"])),
        ("foo.dll", short_import(data, 0, &["_v", "foo.dll"])),
        ("bar.dll", short_import(ordinal, 9, &["g", "bar.dll"])),
    ];
    let long = [("foo.dll", descriptor("foo")), ("foo.dll", long_import("foo", "f", 0, 0))];
    vec![
        ("short imports of two DLLs, indexed", ar(&short, &[("__imp_f", 0), ("f", 0), ("__imp__v", 1), ("__imp_g", 2), ("g", 2)])),
        ("short imports without an index", ar(&short, &[])),
        ("a descriptor and a long import", ar(&long, &[("__IMPORT_DESCRIPTOR_foo", 0), ("__imp_f", 1), ("f", 1)])),
    ]
}

/// A long import whose thunk is `u64::MAX` and whose name symbol is at 1: the reader adds the
/// two as `usize` (`pe/coff.rs` `long_import`), which overflows.
fn thunk_past_usize() -> Vec<u8> {
    let long = [("foo.dll", descriptor("foo")), ("foo.dll", long_import("foo", "f", u64::MAX, 1))];
    ar(&long, &[("__IMPORT_DESCRIPTOR_foo", 0), ("__imp_f", 1)])
}

/// A short import member (`IMPORT_OBJECT_HEADER`) of `types` (its type and name type) and
/// `hint`, its strings the symbol and the DLL.
fn short_import(types: u16, hint: u16, strings: &[&str]) -> Vec<u8> {
    let strings: Vec<u8> = strings.iter().flat_map(|it| it.bytes().chain([0])).collect();
    let mut out = Vec::new();
    for value in [pe::IMAGE_FILE_MACHINE_UNKNOWN.0, 0xffff, 0, pe::IMAGE_FILE_MACHINE_AMD64.0] {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(strings.len() as u32).to_le_bytes());
    out.extend_from_slice(&hint.to_le_bytes());
    out.extend_from_slice(&types.to_le_bytes());
    out.extend_from_slice(&strings);
    out
}

fn define(b: &mut Builder<'_>, name: &str, section: object::write::SectionId, value: u64, scope: SymbolScope) -> object::write::SymbolId {
    b.add_symbol(Symbol {
        name: name.into(),
        value,
        size: 0,
        kind: SymbolKind::Data,
        scope,
        weak: false,
        section: SymbolSection::Section(section),
        flags: SymbolFlags::None,
    })
}

/// `__IMPORT_DESCRIPTOR_<dll>` in `.idata$2`, the DLL's name in `.idata$6`.
fn descriptor(dll: &str) -> Vec<u8> {
    let mut b = Builder::new(BinaryFormat::Coff, Architecture::X86_64, Endianness::Little);
    let directory = b.add_section(Vec::new(), b".idata$2".to_vec(), SectionKind::Data);
    b.append_section_data(directory, &[0; 20], 4);
    let names = b.add_section(Vec::new(), b".idata$6".to_vec(), SectionKind::Data);
    b.append_section_data(names, format!("{dll}.dll\0").as_bytes(), 2);
    define(&mut b, &format!("__IMPORT_DESCRIPTOR_{dll}"), directory, 0, SymbolScope::Linkage);
    b.write().expect("a COFF object")
}

/// A long import member of `symbol` from `dll`'s descriptor: `__imp_<symbol>` in `.idata$5`,
/// whose 8-byte thunk is `thunk` and whose relocation names a static symbol at `value` in
/// `.idata$6`, the hint and name (`0`, `symbol`) there.
fn long_import(dll: &str, symbol: &str, thunk: u64, value: u64) -> Vec<u8> {
    let mut b = Builder::new(BinaryFormat::Coff, Architecture::X86_64, Endianness::Little);
    let thunks = b.add_section(Vec::new(), b".idata$5".to_vec(), SectionKind::Data);
    b.append_section_data(thunks, &thunk.to_le_bytes(), 8);
    let names = b.add_section(Vec::new(), b".idata$6".to_vec(), SectionKind::Data);
    let mut hint_name = vec![0, 0];
    hint_name.extend_from_slice(symbol.as_bytes());
    hint_name.push(0);
    b.append_section_data(names, &hint_name, 2);
    define(&mut b, &format!("__imp_{symbol}"), thunks, 0, SymbolScope::Linkage);
    b.add_symbol(Symbol {
        name: format!("__IMPORT_DESCRIPTOR_{dll}").into_bytes(),
        value: 0,
        size: 0,
        kind: SymbolKind::Unknown,
        scope: SymbolScope::Linkage,
        weak: false,
        section: SymbolSection::Undefined,
        flags: SymbolFlags::None,
    });
    let name = define(&mut b, "hint_name", names, value, SymbolScope::Compilation);
    let flags = RelocationFlags::Coff { typ: pe::IMAGE_REL_AMD64_ADDR32NB };
    b.add_relocation(thunks, Relocation { offset: 0, symbol: name, addend: 0, flags }).expect("a COFF relocation");
    b.write().expect("a COFF object")
}

fn header(out: &mut Vec<u8>, name: &str, size: usize) {
    out.extend_from_slice(format!("{name:<16}{:<12}{:<6}{:<6}{:<8}{size:<10}`\n", 0, 0, 0, 644).as_bytes());
}

/// `members` (names of at most 15 bytes) after a GNU `/` index naming `symbols` (name,
/// member), or none when `symbols` is empty.
fn ar(members: &[(&str, Vec<u8>)], symbols: &[(&str, usize)]) -> Vec<u8> {
    let names: Vec<u8> = symbols.iter().flat_map(|(name, _)| name.bytes().chain([0])).collect();
    let index_len = 4 + 4 * symbols.len() + names.len();
    let mut at = 8;
    if !symbols.is_empty() {
        at += 60 + index_len + index_len % 2;
    }
    let mut offsets = Vec::new();
    for (_, data) in members {
        offsets.push(at);
        at += 60 + data.len() + data.len() % 2;
    }
    let mut out = b"!<arch>\n".to_vec();
    if !symbols.is_empty() {
        header(&mut out, "/", index_len);
        out.extend_from_slice(&(symbols.len() as u32).to_be_bytes());
        for &(_, member) in symbols {
            out.extend_from_slice(&(offsets[member] as u32).to_be_bytes());
        }
        out.extend_from_slice(&names);
        if out.len() % 2 == 1 {
            out.push(b'\n');
        }
    }
    for ((name, data), offset) in members.iter().zip(&offsets) {
        assert_eq!(out.len(), *offset, "the index's offset of member {name}");
        header(&mut out, &format!("{name}/"), data.len());
        out.extend_from_slice(data);
        if out.len() % 2 == 1 {
            out.push(b'\n');
        }
    }
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

    use super::{SHARED, named};

    #[test]
    fn seeds() {
        blaze_fuzzer::seeds(&super::TARGET);
    }

    #[test]
    fn every_case() {
        blaze_fuzzer::every_case(&super::TARGET, super::CASES, super::check_case);
    }

    #[test]
    fn seeds_read() {
        for (what, bytes) in named() {
            assert!(blaze_link::fuzz_read(Format::Pe, Arch::X86_64, SHARED, &bytes), "seed `{what}` is refused");
        }
    }
}
