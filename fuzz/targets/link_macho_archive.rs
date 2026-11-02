//! Static archives of Mach-O objects (BSD `ar` with `#1/<len>` names and a `__.SYMDEF`,
//! `__.SYMDEF SORTED` or `__.SYMDEF_64` index; GNU `ar` with a `/` index), read as the linker
//! reads one on its link line: `blaze_link::fuzz_read(Format::MachO, arch, NativeKind::Archive
//! { whole }, bytes)`, for x86-64 and AArch64. Members are not parsed until the link pulls them.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - the read returns, accepting or refusing, and never panics (a read past the input is a
//!   slice index out of range in the plain test, and ASan's report in the fuzz build);
//! - the largest single allocation of a read is at most `blaze_fuzz::allowed(len)`;
//! - the bytes at an address 8 does not divide read as they do 8-aligned;
//! - `whole` changes nothing the read accepts (the reader needs the index either way).

#![forbid(unsafe_code)]

use std::time::Duration;

use blaze_codegen::Arch;
use blaze_fuzzer::Target;
use blaze_link::{Format, NativeKind};
use object::write::{Object as Builder, StandardSection, Symbol, SymbolSection};
use object::{Architecture, BinaryFormat, Endianness, SymbolFlags, SymbolKind, SymbolScope};

const TARGET: Target = Target {
    name: "link_macho_archive",
    check,
    seeds,
    dictionary: &[
        b"!<arch>\n",
        b"!<thin>\n",
        b"`\n",
        b"#1/",
        b"__.SYMDEF",
        b"__.SYMDEF SORTED",
        b"__.SYMDEF_64",
        b"__.SYMDEF_64 SORTED",
        b"/               ",
        b"//              ",
        b"/SYM64/         ",
        b"\xcf\xfa\xed\xfe",
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
        blaze_fuzz::largest_allocation(|| blaze_link::fuzz_read(Format::MachO, arch, ARCHIVE, aligned));
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
                blaze_link::fuzz_read(Format::MachO, arch, ARCHIVE, shifted),
                read,
                "alignment: a {arch:?} read of the bytes one past an 8-aligned address answers otherwise"
            );
        }
        _ => {
            assert_eq!(
                blaze_link::fuzz_read(Format::MachO, arch, WHOLE, aligned),
                read,
                "whole archive: a {arch:?} read with `whole` answers otherwise"
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
    let mut seeds: Vec<Vec<u8>> = named().into_iter().map(|(_, _, bytes)| bytes).collect();
    seeds.push(one_name_many_times());
    seeds.push(suffixes_of_one_string());
    seeds
}

fn named() -> Vec<(&'static str, Arch, Vec<u8>)> {
    let mut out = Vec::new();
    for (arch, architecture) in [(Arch::X86_64, Architecture::X86_64), (Arch::Aarch64, Architecture::Aarch64)] {
        let members = vec![
            ("a_rather_long_member_name.o", member(architecture, &["one"])),
            ("b.o", member(architecture, &["two", "three"])),
        ];
        let (strings, entries) = index(&[("_one", 0), ("_two", 1), ("_three", 1)]);
        out.push(("a BSD archive with a __.SYMDEF SORTED", arch, bsd(&members, &strings, &entries, false)));
        out.push(("a BSD archive with a __.SYMDEF_64 SORTED", arch, bsd(&members, &strings, &entries, true)));
        let short = [("a.o", members[0].1.clone()), ("b.o", members[1].1.clone())];
        out.push(("a GNU archive with a / index", arch, gnu(&short, &[("_one", 0), ("_two", 1), ("_three", 1)])));
    }
    out.push(("a BSD archive of an empty index and no member", Arch::X86_64, bsd(&[], &[], &[], false)));
    out
}

/// An object defining `_<name>` for each of `names` in its `__text`.
fn member(arch: Architecture, names: &[&str]) -> Vec<u8> {
    let mut b = Builder::new(BinaryFormat::MachO, arch, Endianness::Little);
    let text = b.section_id(StandardSection::Text);
    b.append_section_data(text, &vec![0; 4 * names.len()], 4);
    for (i, name) in names.iter().enumerate() {
        b.add_symbol(Symbol {
            name: name.as_bytes().to_vec(),
            value: 4 * i as u64,
            size: 4,
            kind: SymbolKind::Text,
            scope: SymbolScope::Linkage,
            weak: false,
            section: SymbolSection::Section(text),
            flags: SymbolFlags::None,
        });
    }
    b.write().expect("a Mach-O object")
}

/// A `__.SYMDEF`'s string table of `symbols` (name, member) and its entries (string offset,
/// member).
fn index(symbols: &[(&str, usize)]) -> (Vec<u8>, Vec<(usize, usize)>) {
    let mut strings = Vec::new();
    let mut entries = Vec::new();
    for &(symbol, member) in symbols {
        entries.push((strings.len(), member));
        strings.extend_from_slice(symbol.as_bytes());
        strings.push(0);
    }
    (strings, entries)
}

fn header(out: &mut Vec<u8>, name: &str, size: usize) {
    out.extend_from_slice(format!("{name:<16}{:<12}{:<6}{:<6}{:<8}{size:<10}`\n", 0, 0, 0, 644).as_bytes());
}

/// A BSD member: a `#1/<len>` name padded with NULs to 8 bytes, as Apple's `ar` writes.
fn bsd_member(out: &mut Vec<u8>, name: &str, data: &[u8]) {
    let padded = (name.len() + 1).next_multiple_of(8);
    header(out, &format!("#1/{padded}"), padded + data.len());
    out.extend_from_slice(name.as_bytes());
    out.resize(out.len() + padded - name.len(), 0);
    out.extend_from_slice(data);
    if out.len() % 2 == 1 {
        out.push(b'\n');
    }
}

fn word(out: &mut Vec<u8>, wide: bool, value: usize) {
    if wide {
        out.extend_from_slice(&(value as u64).to_le_bytes());
    } else {
        out.extend_from_slice(&(value as u32).to_le_bytes());
    }
}

/// `members` after a `__.SYMDEF SORTED` (`__.SYMDEF_64 SORTED` when `wide`) of `strings` and
/// `entries` (string offset, member index).
fn bsd(members: &[(&str, Vec<u8>)], strings: &[u8], entries: &[(usize, usize)], wide: bool) -> Vec<u8> {
    let size = if wide { 8 } else { 4 };
    let mut strings = strings.to_vec();
    strings.resize(strings.len().next_multiple_of(size), 0);
    let symdef_name = if wide { "__.SYMDEF_64 SORTED" } else { "__.SYMDEF SORTED" };
    let symdef_len = size + 2 * size * entries.len() + size + strings.len();
    let mut at = 8 + 60 + (symdef_name.len() + 1).next_multiple_of(8) + symdef_len;
    at += at % 2;
    let mut offsets = Vec::new();
    for (name, data) in members {
        offsets.push(at);
        at += 60 + (name.len() + 1).next_multiple_of(8) + data.len();
        at += at % 2;
    }
    let mut symdef = Vec::new();
    word(&mut symdef, wide, 2 * size * entries.len());
    for &(strx, member) in entries {
        word(&mut symdef, wide, strx);
        word(&mut symdef, wide, offsets[member]);
    }
    word(&mut symdef, wide, strings.len());
    symdef.extend_from_slice(&strings);

    let mut out = b"!<arch>\n".to_vec();
    bsd_member(&mut out, symdef_name, &symdef);
    for ((name, data), offset) in members.iter().zip(&offsets) {
        assert_eq!(out.len(), *offset, "the index's offset of member {name}");
        bsd_member(&mut out, name, data);
    }
    out
}

/// `members` (short names, `name/`) after a GNU `/` index naming `symbols` (name, member).
fn gnu(members: &[(&str, Vec<u8>)], symbols: &[(&str, usize)]) -> Vec<u8> {
    let names: Vec<u8> = symbols.iter().flat_map(|(name, _)| name.bytes().chain([0])).collect();
    let index_len = 4 + 4 * symbols.len() + names.len();
    let mut at = 8 + 60 + index_len;
    at += at % 2;
    let mut offsets = Vec::new();
    for (_, data) in members {
        offsets.push(at);
        at += 60 + data.len();
        at += at % 2;
    }
    let mut out = b"!<arch>\n".to_vec();
    header(&mut out, "/", index_len);
    out.extend_from_slice(&(symbols.len() as u32).to_be_bytes());
    for &(_, member) in symbols {
        out.extend_from_slice(&(offsets[member] as u32).to_be_bytes());
    }
    out.extend_from_slice(&names);
    if out.len() % 2 == 1 {
        out.push(b'\n');
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

/// A `__.SYMDEF` of 640 entries that all name one 4096-byte string (about 10 KB in all): one
/// push per entry would make a 2.6 MB name arena, past `allowed(len)` (1.7 MB).
fn one_name_many_times() -> Vec<u8> {
    let mut strings = vec![b'_'];
    strings.resize(4096, b'a');
    strings.push(0);
    let members = [("a.o", member(Architecture::X86_64, &["a"]))];
    bsd(&members, &strings, &[(0, 0); 640], false)
}

/// A `__.SYMDEF` of 640 entries, each naming one 4096-byte string from a different offset:
/// distinct names of 2.4 MB in all from about 10 KB, past `allowed(len)` (1.7 MB). All but the
/// first start with no `_`, so none is a suffix of another's name: the reader refuses the
/// archive before it allocates its names.
fn suffixes_of_one_string() -> Vec<u8> {
    let mut strings = vec![b'_'];
    strings.resize(4096, b'a');
    strings.push(0);
    let members = [("a.o", member(Architecture::X86_64, &["a"]))];
    let entries: Vec<(usize, usize)> = (0..640).map(|at| (at, 0)).collect();
    bsd(&members, &strings, &entries, false)
}

#[cfg_attr(test, allow(dead_code))]
fn main() -> std::process::ExitCode {
    blaze_fuzzer::main(&TARGET)
}

#[cfg(test)]
mod tests {
    use blaze_link::Format;

    use super::{ARCHIVE, named, one_name_many_times, suffixes_of_one_string};

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
            assert!(blaze_link::fuzz_read(Format::MachO, arch, ARCHIVE, &bytes), "seed `{what}` is refused");
        }
        let read = |bytes: &[u8]| {
            blaze_link::fuzz_read(Format::MachO, blaze_codegen::Arch::X86_64, ARCHIVE, bytes)
        };
        assert!(read(&one_name_many_times()));
        assert!(!read(&suffixes_of_one_string()));
    }
}
