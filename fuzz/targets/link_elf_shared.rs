//! ELF shared objects (`ET_DYN`: `.dynsym`, `DT_SONAME`, symbol versions) and the GNU linker
//! scripts glibc ships as `libc.so`, read as the linker reads a shared library of the link
//! line: `blaze_link::fuzz_read(Format::Elf, arch, NativeKind::Shared { as_needed }, bytes)`,
//! for x86-64 and AArch64.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - the read returns, accepting or refusing, and never panics (a read past the input is a
//!   slice index out of range in the plain test, and ASan's report in the fuzz build);
//! - the largest single allocation of a read is at most `blaze_fuzz::allowed(len)`;
//! - the bytes at an address 8 does not divide read as they do 8-aligned;
//! - `as_needed` changes nothing the read accepts;
//! - an accepted ELF file (not a script, whose text runs to the end) is still accepted with 64
//!   bytes after its end: every structure is named by an offset and a size.

#![forbid(unsafe_code)]

use std::time::Duration;

use blaze_codegen::Arch;
use blaze_fuzzer::Target;
use blaze_link::{Format, NativeKind};
use object::write::{Object as Builder, StandardSection, Symbol, SymbolSection};
use object::{Architecture, BinaryFormat, Endianness, SectionFlags, SectionKind, SymbolFlags, SymbolKind, SymbolScope, elf};

const TARGET: Target = Target {
    name: "link_elf_shared",
    check,
    seeds,
    dictionary: &[
        b"\x7fELF\x02\x01\x01",
        b"\x03\x00",
        b"GROUP",
        b"INPUT",
        b"AS_NEEDED",
        b"OUTPUT_FORMAT",
        b"OUTPUT_ARCH",
        b"SEARCH_DIR",
        b"/*",
        b"*/",
        b"-l",
        b"\"",
        b".dynsym",
        b".dynstr",
        b".dynamic",
        b".gnu.version",
        b".gnu.version_d",
        b".gnu.version_r",
        b"GLIBC_2.2.5",
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(600),
};

const SHARED: NativeKind = NativeKind::Shared { as_needed: false };
const AS_NEEDED: NativeKind = NativeKind::Shared { as_needed: true };
const ARCHES: [Arch; 2] = [Arch::X86_64, Arch::Aarch64];

/// An arch and the read compared with the first (shifted, as_needed, padded), one a run
/// (`blaze_fuzzer::case`); the plain test takes every one of each seed.
const CASES: usize = ARCHES.len() * 3;

fn check(data: &[u8]) {
    check_case(data, blaze_fuzzer::case(data, CASES));
}

fn check_case(data: &[u8], case: usize) {
    let arch = ARCHES[case % ARCHES.len()];
    let (aligned, at) = copy_at(data, 0, 0);
    let aligned = &aligned[at..][..data.len()];
    let bound = blaze_fuzz::allowed(data.len());
    let (largest, read) =
        blaze_fuzz::largest_allocation(|| blaze_link::fuzz_read(Format::Elf, arch, SHARED, aligned));
    assert!(
        largest <= bound,
        "allocation bound: a {arch:?} read of {} bytes asked for {largest} bytes at once, past {bound}",
        data.len()
    );
    match case / ARCHES.len() {
        0 => {
            let (shifted, at) = copy_at(data, 1, 0);
            let shifted = &shifted[at..][..data.len()];
            assert_eq!(
                blaze_link::fuzz_read(Format::Elf, arch, SHARED, shifted),
                read,
                "alignment: a {arch:?} read of the bytes one past an 8-aligned address answers otherwise"
            );
        }
        1 => {
            assert_eq!(
                blaze_link::fuzz_read(Format::Elf, arch, AS_NEEDED, aligned),
                read,
                "as needed: a {arch:?} read with `as_needed` answers otherwise"
            );
        }
        _ => {
            if read && data.starts_with(&elf::ELFMAG) {
                let (padded, at) = copy_at(data, 0, 64);
                let padded = &padded[at..][..data.len() + 64];
                assert!(
                    blaze_link::fuzz_read(Format::Elf, arch, SHARED, padded),
                    "inside the input: a {arch:?} read accepted the bytes and refuses them with 64 more after"
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
    named().into_iter().map(|(_, _, bytes)| bytes).collect()
}

fn named() -> Vec<(&'static str, Arch, Vec<u8>)> {
    vec![
        ("an x86-64 shared object with a soname", Arch::X86_64, shared(Architecture::X86_64, true)),
        ("an AArch64 shared object with a soname", Arch::Aarch64, shared(Architecture::Aarch64, true)),
        ("an x86-64 shared object without .dynamic", Arch::X86_64, shared(Architecture::X86_64, false)),
        ("glibc's libc.so", Arch::X86_64, LIBC.as_bytes().to_vec()),
        ("a script of every command read", Arch::Aarch64, SCRIPT.as_bytes().to_vec()),
    ]
}

const LIBC: &str = "/* GNU ld script\n   Use the shared library, but some functions are only in\n   the static library, so try that secondarily.  */\nOUTPUT_FORMAT(elf64-x86-64)\nGROUP ( /lib/x86_64-linux-gnu/libc.so.6 /usr/lib/x86_64-linux-gnu/libc_nonshared.a  AS_NEEDED ( /lib64/ld-linux-x86-64.so.2 ) )\n";

const SCRIPT: &str = "/* two\n lines */\nOUTPUT_FORMAT(\"elf64-x86-64\", \"elf64-x86-64\",\n \"elf64-x86-64\")\nOUTPUT_ARCH(i386:x86-64);\nGROUP ( /lib/a.so.1, /usr/lib/b_nonshared.a  AS_NEEDED ( /lib64/ld.so.2 ) )\nINPUT(c.o \"d e.o\")\n";

const SONAME: &[u8] = b"libfuzz.so.1";

/// A shared object made of `object`'s relocatable one: `e_type` made `ET_DYN`, `.symtab`
/// made `.dynsym` (its string table is the dynamic one), and with `soname` a `.dynamic` whose
/// `DT_SONAME` names a string of that table.
fn shared(arch: Architecture, soname: bool) -> Vec<u8> {
    let mut b = Builder::new(BinaryFormat::Elf, arch, Endianness::Little);
    let text = b.section_id(StandardSection::Text);
    b.append_section_data(text, &[0; 16], 16);
    let data = b.section_id(StandardSection::Data);
    b.append_section_data(data, &[0; 8], 8);
    let define = |name: &str, section, value, kind, weak| Symbol {
        name: name.into(),
        value,
        size: 4,
        kind,
        scope: SymbolScope::Dynamic,
        weak,
        section: SymbolSection::Section(section),
        flags: SymbolFlags::None,
    };
    b.add_symbol(define("f", text, 0, SymbolKind::Text, false));
    b.add_symbol(define("g", text, 8, SymbolKind::Text, true));
    b.add_symbol(define("v", data, 0, SymbolKind::Data, false));
    b.add_symbol(Symbol {
        name: SONAME.to_vec(),
        value: 0,
        size: 0,
        kind: SymbolKind::Unknown,
        scope: SymbolScope::Dynamic,
        weak: false,
        section: SymbolSection::Undefined,
        flags: SymbolFlags::None,
    });
    if soname {
        let dynamic = b.add_section(Vec::new(), b".dynamic".to_vec(), SectionKind::Other);
        b.section_mut(dynamic).flags = SectionFlags::Elf { sh_type: elf::SHT_DYNAMIC, sh_flags: elf::SectionFlags(0) };
        let mut entries = Vec::new();
        for value in [elf::DT_SONAME.0 as u64, 0, elf::DT_NULL.0 as u64, 0] {
            entries.extend_from_slice(&value.to_le_bytes());
        }
        b.append_section_data(dynamic, &entries, 8);
    }
    let mut bytes = b.write().expect("an ELF object");

    bytes[16..18].copy_from_slice(&elf::ET_DYN.0.to_le_bytes());
    let headers = section_headers(&bytes);
    let symtab = headers.iter().find(|it| it.ty == elf::SHT_SYMTAB.0).expect("a .symtab").at;
    bytes[symtab + 4..symtab + 8].copy_from_slice(&elf::SHT_DYNSYM.0.to_le_bytes());
    let strtab = u32::from_le_bytes(bytes[symtab + 40..symtab + 44].try_into().expect("four bytes"));
    if soname {
        let strings = &headers[strtab as usize];
        let table = &bytes[strings.offset..strings.offset + strings.size];
        let mut needle = vec![0];
        needle.extend_from_slice(SONAME);
        needle.push(0);
        let found = table.windows(needle.len()).position(|it| it == needle).expect("the soname's string") + 1;
        let dynamic = headers.iter().find(|it| it.ty == elf::SHT_DYNAMIC.0).expect("a .dynamic");
        let (at, offset) = (dynamic.at, dynamic.offset);
        bytes[at + 40..at + 44].copy_from_slice(&strtab.to_le_bytes());
        bytes[offset + 8..offset + 16].copy_from_slice(&(found as u64).to_le_bytes());
    }
    bytes
}

struct Header {
    /// Where the section header is in the file.
    at: usize,
    ty: u32,
    offset: usize,
    size: usize,
}

/// The section headers of a 64-bit little-endian ELF file `object` wrote.
fn section_headers(bytes: &[u8]) -> Vec<Header> {
    let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("eight bytes"));
    let shoff = u64_at(40) as usize;
    let shnum = usize::from(u16::from_le_bytes([bytes[60], bytes[61]]));
    (0..shnum)
        .map(|index| {
            let at = shoff + 64 * index;
            Header {
                at,
                ty: u32::from_le_bytes(bytes[at + 4..at + 8].try_into().expect("four bytes")),
                offset: u64_at(at + 24) as usize,
                size: u64_at(at + 32) as usize,
            }
        })
        .collect()
}

#[cfg_attr(test, allow(dead_code))]
fn main() -> std::process::ExitCode {
    blaze_fuzzer::main(&TARGET)
}

#[cfg(test)]
mod tests {
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
        for (what, arch, bytes) in named() {
            assert!(blaze_link::fuzz_read(Format::Elf, arch, SHARED, &bytes), "seed `{what}` is refused");
        }
    }
}
