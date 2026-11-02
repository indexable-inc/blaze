//! COFF relocatable objects (x86-64, regular and `/bigobj`) and short import members, read as
//! the linker reads one on its link line: `blaze_link::fuzz_read(Format::Pe, Arch::X86_64,
//! NativeKind::Object, bytes)`. The PE writer is x86-64 only.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - the read returns, accepting or refusing, and never panics (a read past the input is a
//!   slice index out of range in the plain test, and ASan's report in the fuzz build);
//! - the largest single allocation of a read is at most `blaze_fuzz::allowed(len)`;
//! - the bytes at an address 8 does not divide read as they do 8-aligned (the reader copies
//!   them to read `object`'s headers in place);
//! - an accepted object is still accepted with 64 bytes after its end: its sections, symbols,
//!   strings and relocations, and an import member's strings, are named by an offset and a size.

#![forbid(unsafe_code)]

use std::time::Duration;

use blaze_codegen::Arch;
use blaze_fuzzer::Target;
use blaze_link::{Format, NativeKind};
use object::write::{Comdat, Object as Builder, Relocation, StandardSection, Symbol, SymbolId, SymbolSection};
use object::{
    Architecture, BinaryFormat, ComdatKind, Endianness, RelocationEncoding, RelocationFlags, RelocationKind,
    SectionKind, SymbolFlags, SymbolKind, SymbolScope, pe,
};

const TARGET: Target = Target {
    name: "link_coff_object",
    check,
    seeds,
    dictionary: &[
        b"\x64\x86",
        b"\x00\x00\xff\xff",
        b"\x00\x00\xff\xff\x00\x00\x64\x86",
        b"\x00\x00\xff\xff\x02\x00\x64\x86",
        b"\xc7\xa1\xba\xd1\xee\xba\xa9\x4b\xaf\x20\xfa\xf6\x6a\xa4\xdc\xb8",
        b".text\0\0\0",
        b".data\0\0\0",
        b".rdata\0\0",
        b".bss\0\0\0\0",
        b".pdata\0\0",
        b".xdata\0\0",
        b".tls$\0\0\0",
        b".CRT$XCU",
        b".drectve",
        b".idata$5",
        b".idata$6",
        b"/DEFAULTLIB:",
        b"/EXPORT:",
        b"/ALTERNATENAME:",
        b"/INCLUDE:",
        b"/FAILIFMISMATCH:",
        b"/NODEFAULTLIB",
        b"\x02\x00\x00\x00",
        b"\x67\x00",
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(600),
};

const OBJECT: NativeKind = NativeKind::Object;

/// The read compared with the first (shifted, padded), one a run
/// (`blaze_fuzzer::case`); the plain test takes every one of each seed.
const CASES: usize = 2;

fn check(data: &[u8]) {
    check_case(data, blaze_fuzzer::case(data, CASES));
}

fn check_case(data: &[u8], case: usize) {
    let (aligned, at) = copy_at(data, 0, 0);
    let aligned = &aligned[at..][..data.len()];
    let bound = blaze_fuzz::allowed(data.len());
    let (largest, read) =
        blaze_fuzz::largest_allocation(|| blaze_link::fuzz_read(Format::Pe, Arch::X86_64, OBJECT, aligned));
    assert!(
        largest <= bound,
        "allocation bound: a read of {} bytes asked for {largest} bytes at once, past {bound}",
        data.len()
    );
    match case {
        0 => {
            let (shifted, at) = copy_at(data, 1, 0);
            let shifted = &shifted[at..][..data.len()];
            assert_eq!(
                blaze_link::fuzz_read(Format::Pe, Arch::X86_64, OBJECT, shifted),
                read,
                "alignment: a read of the bytes one past an 8-aligned address answers otherwise"
            );
        }
        _ => {
            if read {
                let (padded, at) = copy_at(data, 0, 64);
                let padded = &padded[at..][..data.len() + 64];
                assert!(
                    blaze_link::fuzz_read(Format::Pe, Arch::X86_64, OBJECT, padded),
                    "inside the input: a read accepted the bytes and refuses them with 64 more after"
                );
            }
        }
    }
}

/// `data` copied to `shift` past an 8-aligned address of a buffer with `extra` bytes of `0xff`
/// after it, and where it starts in the buffer.
fn copy_at(data: &[u8], shift: usize, extra: usize) -> (Vec<u8>, usize) {
    let mut buffer = vec![0xff; data.len() + extra + 16];
    let start = (8 - buffer.as_ptr() as usize % 8) % 8 + shift;
    buffer[start..start + data.len()].copy_from_slice(data);
    (buffer, start)
}

fn seeds() -> Vec<Vec<u8>> {
    named().into_iter().map(|(_, bytes)| bytes).collect()
}

fn named() -> Vec<(&'static str, Vec<u8>)> {
    let code = pe::IMPORT_OBJECT_CODE.0 | (pe::IMPORT_OBJECT_NAME.0 << 2);
    let data = pe::IMPORT_OBJECT_DATA.0 | (pe::IMPORT_OBJECT_NAME_UNDECORATE.0 << 2);
    let ordinal = pe::IMPORT_OBJECT_CODE.0 | (pe::IMPORT_OBJECT_ORDINAL.0 << 2);
    let export_as = pe::IMPORT_OBJECT_CODE.0 | (pe::IMPORT_OBJECT_NAME_EXPORTAS.0 << 2);
    vec![
        ("an object of every section kind and relocation read", object()),
        ("an object of one function", minimal()),
        ("a short import of a function by name", short_import(code, 7, &["f", "foo.dll"])),
        ("a short import of data, its name undecorated", short_import(data, 0, &["_v@4", "foo.dll"])),
        ("a short import by ordinal", short_import(ordinal, 12, &["g", "bar.dll"])),
        ("a short import exported as another name", short_import(export_as, 0, &["h", "bar.dll", "hh"])),
    ]
}

/// `f` calls the undefined `ext` and takes `v`'s address; `.data` holds `&f`, `.rdata` `f`'s
/// image-relative address; `.bss`; a COMDAT `.text$mn` defining `inline` with an associative
/// `.xdata`; `.drectve` options.
fn object() -> Vec<u8> {
    let mut b = Builder::new(BinaryFormat::Coff, Architecture::X86_64, Endianness::Little);
    let text = b.section_id(StandardSection::Text);
    let mut code = vec![0xe8, 0, 0, 0, 0, 0x48, 0x8d, 0x05, 0, 0, 0, 0, 0xc3];
    code.resize(16, 0xcc);
    b.append_section_data(text, &code, 16);
    let data = b.section_id(StandardSection::Data);
    b.append_section_data(data, &[0; 16], 8);
    let rdata = b.section_id(StandardSection::ReadOnlyData);
    b.append_section_data(rdata, &[0; 4], 4);
    let bss = b.section_id(StandardSection::UninitializedData);
    b.append_section_bss(bss, 32, 8);
    let inline = b.add_section(Vec::new(), b".text$mn".to_vec(), SectionKind::Text);
    b.append_section_data(inline, &[0xc3], 16);
    let xdata = b.add_section(Vec::new(), b".xdata".to_vec(), SectionKind::ReadOnlyData);
    b.append_section_data(xdata, &[1, 0, 0, 0], 4);
    // A COMDAT section's first symbol is its section symbol (with the selection), the second
    // the group's.
    b.section_symbol(inline);
    b.section_symbol(xdata);
    let drectve = b.add_section(Vec::new(), b".drectve".to_vec(), SectionKind::Linker);
    b.append_section_data(drectve, b" /DEFAULTLIB:kernel32 /EXPORT:f /ALTERNATENAME:a=f /INCLUDE:f", 1);

    let mut define = |name: &str, section, value, kind| {
        b.add_symbol(Symbol {
            name: name.into(),
            value,
            size: 0,
            kind,
            scope: SymbolScope::Linkage,
            weak: false,
            section: SymbolSection::Section(section),
            flags: SymbolFlags::None,
        })
    };
    let f = define("f", text, 0, SymbolKind::Text);
    let v = define("v", data, 0, SymbolKind::Data);
    define("z", bss, 0, SymbolKind::Data);
    let leader = define("inline", inline, 0, SymbolKind::Text);
    let ext = b.add_symbol(Symbol {
        name: b"ext".to_vec(),
        value: 0,
        size: 0,
        kind: SymbolKind::Text,
        scope: SymbolScope::Linkage,
        weak: false,
        section: SymbolSection::Undefined,
        flags: SymbolFlags::None,
    });
    b.add_comdat(Comdat { kind: ComdatKind::Any, symbol: leader, sections: vec![inline, xdata] });

    let mut relocate = |section, offset, symbol: SymbolId, flags, addend| {
        b.add_relocation(section, Relocation { offset, symbol, addend, flags }).expect("a COFF relocation");
    };
    let rel32 = RelocationFlags::Generic { kind: RelocationKind::Relative, encoding: RelocationEncoding::Generic, size: 32 };
    let abs64 = RelocationFlags::Generic { kind: RelocationKind::Absolute, encoding: RelocationEncoding::Generic, size: 64 };
    relocate(text, 1, ext, rel32, -4);
    relocate(text, 8, v, rel32, -4);
    relocate(data, 0, f, abs64, 0);
    relocate(rdata, 0, f, RelocationFlags::Coff { typ: pe::IMAGE_REL_AMD64_ADDR32NB }, 0);
    relocate(xdata, 0, leader, RelocationFlags::Coff { typ: pe::IMAGE_REL_AMD64_ADDR32NB }, 0);
    b.write().expect("a COFF object")
}

fn minimal() -> Vec<u8> {
    let mut b = Builder::new(BinaryFormat::Coff, Architecture::X86_64, Endianness::Little);
    let text = b.section_id(StandardSection::Text);
    b.append_section_data(text, &[0xc3], 16);
    b.add_symbol(Symbol {
        name: b"main".to_vec(),
        value: 0,
        size: 0,
        kind: SymbolKind::Text,
        scope: SymbolScope::Linkage,
        weak: false,
        section: SymbolSection::Section(text),
        flags: SymbolFlags::None,
    });
    b.write().expect("a COFF object")
}

/// A short import member (`IMPORT_OBJECT_HEADER`) of `types` (its type and name type) and
/// `hint`, its strings the symbol, the DLL and (for `EXPORTAS`) the export name.
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
    fn every_case() {
        blaze_fuzzer::every_case(&super::TARGET, super::CASES, super::check_case);
    }

    #[test]
    fn seeds_read() {
        for (what, bytes) in named() {
            assert!(blaze_link::fuzz_read(Format::Pe, Arch::X86_64, OBJECT, &bytes), "seed `{what}` is refused");
        }
    }
}
