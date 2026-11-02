//! `ar` archives of ELF objects (GNU, GNU 64-bit, BSD), read as the linker reads a `.a` of the
//! link line: `blaze_link::fuzz_read(Format::Elf, arch, NativeKind::Archive { whole }, bytes)`,
//! for x86-64 and AArch64. Without a symbol table the reader indexes every member by reading
//! its symbols, so this reaches the object reader too.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - the read returns, accepting or refusing, and never panics (a read past the input is a
//!   slice index out of range in the plain test, and ASan's report in the fuzz build);
//! - the largest single allocation of the read on this thread is at most
//!   `blaze_fuzz::allowed(len)` (members of an archive without an index are read on rayon's
//!   threads, which the counting allocator does not count here);
//! - the bytes at an address 8 does not divide read as they do 8-aligned;
//! - `--whole-archive` (`whole: true`) answers as a plain archive: the reader takes both alike.

#![forbid(unsafe_code)]

use std::time::Duration;

use blaze_codegen::Arch;
use blaze_fuzzer::Target;
use blaze_link::{Format, NativeKind};
use object::write::{Object as Builder, Relocation, StandardSection, Symbol, SymbolSection};
use object::{Architecture, BinaryFormat, Endianness, RelocationFlags, SymbolFlags, SymbolKind, SymbolScope, elf};

const TARGET: Target = Target {
    name: "link_elf_archive",
    check,
    seeds,
    dictionary: &[
        b"!<arch>\n",
        b"!<thin>\n",
        b"`\n",
        b"/               ",
        b"//              ",
        b"/SYM64/         ",
        b"__.SYMDEF       ",
        b"#1/",
        b"/0              ",
        b"0           0     0     644     ",
        b"\x7fELF\x02\x01\x01",
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(600),
};

const ARCHIVE: NativeKind = NativeKind::Archive { whole: false };
const WHOLE: NativeKind = NativeKind::Archive { whole: true };
const ARCHES: [Arch; 2] = [Arch::X86_64, Arch::Aarch64];

/// An arch and the read compared with the first (shifted, whole), one a run
/// (`blaze_fuzzer::case`); the plain test takes every one of each seed.
const CASES: usize = ARCHES.len() * 2;

fn check(data: &[u8]) {
    check_case(data, blaze_fuzzer::case(data, CASES));
}

fn check_case(data: &[u8], case: usize) {
    let arch = ARCHES[case % ARCHES.len()];
    let (aligned, at) = copy_at(data, 0);
    let aligned = &aligned[at..][..data.len()];
    let bound = blaze_fuzz::allowed(data.len());
    let (largest, read) =
        blaze_fuzz::largest_allocation(|| blaze_link::fuzz_read(Format::Elf, arch, ARCHIVE, aligned));
    assert!(
        largest <= bound,
        "allocation bound: a {arch:?} read of {} bytes asked for {largest} bytes at once, past {bound}",
        data.len()
    );
    match case / ARCHES.len() {
        0 => {
            let (shifted, at) = copy_at(data, 1);
            let shifted = &shifted[at..][..data.len()];
            assert_eq!(
                blaze_link::fuzz_read(Format::Elf, arch, ARCHIVE, shifted),
                read,
                "alignment: a {arch:?} read of the bytes one past an 8-aligned address answers otherwise"
            );
        }
        _ => {
            assert_eq!(
                blaze_link::fuzz_read(Format::Elf, arch, WHOLE, aligned),
                read,
                "whole archive: a {arch:?} read taken whole answers otherwise than taken for its symbols"
            );
        }
    }
}

/// `data` copied to `shift` past an 8-aligned address, and where it starts in the buffer.
fn copy_at(data: &[u8], shift: usize) -> (Vec<u8>, usize) {
    let mut buffer = vec![0xff; data.len() + 16];
    let start = (8 - buffer.as_ptr() as usize % 8) % 8 + shift;
    buffer[start..start + data.len()].copy_from_slice(data);
    (buffer, start)
}

/// Archives of `object`'s ELF objects: with a GNU and a GNU 64-bit symbol table, a long-name
/// table, and none.
fn seeds() -> Vec<Vec<u8>> {
    named().into_iter().map(|(_, _, bytes)| bytes).collect()
}

fn named() -> Vec<(&'static str, Arch, Vec<u8>)> {
    let mut out = Vec::new();
    for (arch, architecture) in [(Arch::X86_64, Architecture::X86_64), (Arch::Aarch64, Architecture::Aarch64)] {
        let (a, b) = members(architecture);
        let symbols: &[(&str, usize)] = &[("alpha", 0), ("beta", 1), ("gamma", 1)];
        let members: &[(&str, &[u8])] = &[("a.o", &a), ("b.o", &b)];
        let long: &[(&str, &[u8])] = &[("a_member_with_a_long_name.o", &a), ("b.o", &b)];
        out.push(("a GNU symbol table", arch, ar(Some(symbols), false, members)));
        out.push(("a GNU 64-bit symbol table", arch, ar(Some(symbols), true, members)));
        out.push(("a long-name table", arch, ar(Some(symbols), false, long)));
        out.push(("no symbol table", arch, ar(None, false, members)));
    }
    out.push(("no members", Arch::X86_64, b"!<arch>\n".to_vec()));
    out
}

fn members(arch: Architecture) -> (Vec<u8>, Vec<u8>) {
    let (call, addend) = match arch {
        Architecture::Aarch64 => (elf::R_AARCH64_CALL26, 0),
        _ => (elf::R_X86_64_PLT32, -4),
    };
    let symbol = |name: &str, section, value, scope, weak| Symbol {
        name: name.into(),
        value,
        size: 4,
        kind: SymbolKind::Text,
        scope,
        weak,
        section,
        flags: SymbolFlags::None,
    };
    let mut a = Builder::new(BinaryFormat::Elf, arch, Endianness::Little);
    let text = a.section_id(StandardSection::Text);
    a.append_section_data(text, &[0; 8], 16);
    a.add_symbol(symbol("alpha", SymbolSection::Section(text), 0, SymbolScope::Dynamic, false));
    let beta = a.add_symbol(Symbol {
        size: 0,
        kind: SymbolKind::Unknown,
        ..symbol("beta", SymbolSection::Undefined, 0, SymbolScope::Dynamic, false)
    });
    a.add_relocation(text, Relocation { offset: 4, symbol: beta, addend, flags: RelocationFlags::Elf { r_type: call } })
        .expect("a relocation inside its section");
    let mut b = Builder::new(BinaryFormat::Elf, arch, Endianness::Little);
    let text = b.section_id(StandardSection::Text);
    b.append_section_data(text, &[0; 8], 16);
    b.add_symbol(symbol("local_b", SymbolSection::Section(text), 0, SymbolScope::Compilation, false));
    b.add_symbol(symbol("beta", SymbolSection::Section(text), 0, SymbolScope::Dynamic, false));
    b.add_symbol(symbol("gamma", SymbolSection::Section(text), 4, SymbolScope::Dynamic, true));
    (a.write().expect("an ELF object"), b.write().expect("an ELF object"))
}

fn header(out: &mut Vec<u8>, name: &str, size: usize) {
    let header = format!("{name:<16}{:<12}{:<6}{:<6}{:<8}{size:<10}`\n", 0, 0, 0, 644);
    assert_eq!(header.len(), 60, "an ar member header");
    out.extend_from_slice(header.as_bytes());
}

fn pad(out: &mut Vec<u8>) {
    if out.len() % 2 == 1 {
        out.push(b'\n');
    }
}

/// A GNU archive of `members`, with a `/` (or, `wide`, a `/SYM64/`) symbol table of `symbols`
/// (each a name and the member defining it) when given, and a `//` table for names longer
/// than a header holds.
fn ar(symbols: Option<&[(&str, usize)]>, wide: bool, members: &[(&str, &[u8])]) -> Vec<u8> {
    let word = if wide { 8 } else { 4 };
    let mut names = Vec::new();
    let mut headers = Vec::new();
    for (name, _) in members {
        if name.len() < 16 {
            headers.push(format!("{name}/"));
        } else {
            headers.push(format!("/{}", names.len()));
            names.extend_from_slice(name.as_bytes());
            names.extend_from_slice(b"/\n");
        }
    }
    let table = symbols.map(|symbols| {
        let strings: usize = symbols.iter().map(|(name, _)| name.len() + 1).sum();
        (symbols, word + word * symbols.len() + strings)
    });
    let mut offset = 8 + table.map_or(0, |(_, size)| 60 + size + size % 2);
    if !names.is_empty() {
        offset += 60 + names.len() + names.len() % 2;
    }
    let mut offsets = Vec::new();
    for (_, bytes) in members {
        offsets.push(offset);
        offset += 60 + bytes.len() + bytes.len() % 2;
    }
    let mut out = b"!<arch>\n".to_vec();
    if let Some((symbols, size)) = table {
        header(&mut out, if wide { "/SYM64/" } else { "/" }, size);
        let number = |out: &mut Vec<u8>, value: usize| {
            if wide {
                out.extend_from_slice(&(value as u64).to_be_bytes());
            } else {
                out.extend_from_slice(&(value as u32).to_be_bytes());
            }
        };
        number(&mut out, symbols.len());
        for (_, member) in symbols {
            number(&mut out, offsets[*member]);
        }
        for (name, _) in symbols {
            out.extend_from_slice(name.as_bytes());
            out.push(0);
        }
        pad(&mut out);
    }
    if !names.is_empty() {
        header(&mut out, "//", names.len());
        out.extend_from_slice(&names);
        pad(&mut out);
    }
    for (((_, bytes), name), offset) in members.iter().zip(&headers).zip(&offsets) {
        assert_eq!(out.len(), *offset, "the symbol table's offset of member {name}");
        header(&mut out, name, bytes.len());
        out.extend_from_slice(bytes);
        pad(&mut out);
    }
    out
}

#[cfg_attr(test, allow(dead_code))]
fn main() -> std::process::ExitCode {
    blaze_fuzzer::main(&TARGET)
}

#[cfg(test)]
mod tests {
    use blaze_link::Format;

    use super::{ARCHIVE, named};

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
            assert!(blaze_link::fuzz_read(Format::Elf, arch, ARCHIVE, &bytes), "seed `{what}` is refused");
        }
    }
}
