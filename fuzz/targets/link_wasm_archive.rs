//! `ar` archives of wasm objects (as `llvm-ar` writes them: GNU with a `/` index, or BSD with
//! a `__.SYMDEF`), read as the linker reads one on its link line:
//! `blaze_link::fuzz_read(Format::Wasm, Arch::Wasm32, NativeKind::Archive { whole }, bytes)`.
//! Members are not parsed until the link pulls them.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - the read returns, accepting or refusing, and never panics (a read past the input is a
//!   slice index out of range in the plain test, and ASan's report in the fuzz build);
//! - the largest single allocation of a read is at most `blaze_fuzz::allowed(len)`;
//! - the bytes at an address 8 does not divide read as they do 8-aligned;
//! - what is accepted without `whole` is accepted with it (only an archive without an index
//!   needs `whole`).

#![forbid(unsafe_code)]

use std::time::Duration;

use blaze_codegen::Arch;
use blaze_fuzzer::Target;
use blaze_link::{Format, NativeKind};

const TARGET: Target = Target {
    name: "link_wasm_archive",
    check,
    seeds,
    dictionary: &[
        b"!<arch>\n",
        b"!<thin>\n",
        b"`\n",
        b"#1/",
        b"__.SYMDEF",
        b"__.SYMDEF_64",
        b"/               ",
        b"//              ",
        b"/SYM64/         ",
        b"\0asm\x01\0\0\0",
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(600),
};

const ARCHIVE: NativeKind = NativeKind::Archive { whole: false };
const WHOLE: NativeKind = NativeKind::Archive { whole: true };

/// The read compared with the first (shifted, whole), one a run
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
        blaze_fuzz::largest_allocation(|| blaze_link::fuzz_read(Format::Wasm, Arch::Wasm32, ARCHIVE, aligned));
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
                blaze_link::fuzz_read(Format::Wasm, Arch::Wasm32, ARCHIVE, shifted),
                read,
                "alignment: a read of the bytes one past an 8-aligned address answers otherwise"
            );
        }
        _ => {
            let whole = blaze_link::fuzz_read(Format::Wasm, Arch::Wasm32, WHOLE, aligned);
            assert!(!read || whole, "whole archive: an archive accepted without `whole` is refused with it");
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
    seeds.push(one_name_many_times());
    seeds
}

/// Each accepted with `whole`; all but the one without an index without it too.
fn named() -> Vec<(&'static str, Vec<u8>)> {
    let members = [("a.o", object(&["alpha", "beta"])), ("b.o", object(&["gamma"]))];
    let (strings, entries) = index(&[("alpha", 0), ("beta", 0), ("gamma", 1)]);
    vec![
        ("a GNU archive with a / index", gnu(&members, &[("alpha", 0), ("beta", 0), ("gamma", 1)])),
        ("a BSD archive with a __.SYMDEF", bsd(&members, &strings, &entries)),
        ("a GNU archive without an index", gnu(&members, &[])),
        ("no member", b"!<arch>\n".to_vec()),
    ]
}

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

fn section(out: &mut Vec<u8>, id: u8, contents: &[u8]) {
    out.push(id);
    uleb(out, contents.len() as u64);
    out.extend(contents);
}

/// A wasm object of one function `() -> ()` for each of `functions`, each with its symbol.
fn object(functions: &[&str]) -> Vec<u8> {
    let count = functions.len() as u8;
    let mut out = b"\0asm\x01\0\0\0".to_vec();
    section(&mut out, 1, &[1, 0x60, 0, 0]);
    let mut types = vec![count];
    types.resize(1 + functions.len(), 0);
    section(&mut out, 3, &types);
    let mut code = vec![count];
    for _ in functions {
        code.extend([2, 0x00, 0x0b]);
    }
    section(&mut out, 10, &code);
    let mut table = vec![count];
    for (i, function) in functions.iter().enumerate() {
        table.extend([0, 0, i as u8, function.len() as u8]);
        table.extend(function.as_bytes());
    }
    let mut linking = vec![2, 8];
    uleb(&mut linking, table.len() as u64);
    linking.extend(table);
    let mut custom = vec![7];
    custom.extend(b"linking");
    custom.extend(linking);
    section(&mut out, 0, &custom);
    out
}

fn header(out: &mut Vec<u8>, name: &str, size: usize) {
    out.extend_from_slice(format!("{name:<16}{:<12}{:<6}{:<6}{:<8}{size:<10}`\n", 0, 0, 0, 644).as_bytes());
}

/// `members` (names of at most 15 bytes) after a GNU `/` index naming `symbols` (name,
/// member), or none when `symbols` is empty.
fn gnu(members: &[(&str, Vec<u8>)], symbols: &[(&str, usize)]) -> Vec<u8> {
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

/// A BSD member: a `#1/<len>` name padded with NULs to 8 bytes.
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

/// `members` after a `__.SYMDEF` of `strings` and `entries` (string offset, member index).
fn bsd(members: &[(&str, Vec<u8>)], strings: &[u8], entries: &[(usize, usize)]) -> Vec<u8> {
    let mut strings = strings.to_vec();
    strings.resize(strings.len().next_multiple_of(4), 0);
    let symdef_name = "__.SYMDEF";
    let symdef_len = 4 + 8 * entries.len() + 4 + strings.len();
    let mut at = 8 + 60 + (symdef_name.len() + 1).next_multiple_of(8) + symdef_len;
    at += at % 2;
    let mut offsets = Vec::new();
    for (name, data) in members {
        offsets.push(at);
        at += 60 + (name.len() + 1).next_multiple_of(8) + data.len();
        at += at % 2;
    }
    let mut symdef = Vec::new();
    symdef.extend_from_slice(&(8 * entries.len() as u32).to_le_bytes());
    for &(strx, member) in entries {
        symdef.extend_from_slice(&(strx as u32).to_le_bytes());
        symdef.extend_from_slice(&(offsets[member] as u32).to_le_bytes());
    }
    symdef.extend_from_slice(&(strings.len() as u32).to_le_bytes());
    symdef.extend_from_slice(&strings);

    let mut out = b"!<arch>\n".to_vec();
    bsd_member(&mut out, symdef_name, &symdef);
    for ((name, data), offset) in members.iter().zip(&offsets) {
        assert_eq!(out.len(), *offset, "the index's offset of member {name}");
        bsd_member(&mut out, name, data);
    }
    out
}

/// A `__.SYMDEF` of 640 entries that all name one 4096-byte string (about 10 KB in all): the
/// reader pushes each entry's name into one name arena, 2.6 MB, past `allowed(len)` (1.7 MB),
/// and quadratic in the input.
fn one_name_many_times() -> Vec<u8> {
    let mut strings = vec![b'a'; 4096];
    strings.push(0);
    let members = [("a.o", object(&["a"]))];
    bsd(&members, &strings, &[(0, 0); 640])
}

#[cfg_attr(test, allow(dead_code))]
fn main() -> std::process::ExitCode {
    blaze_fuzzer::main(&TARGET)
}

#[cfg(test)]
mod tests {
    use blaze_codegen::Arch;
    use blaze_link::Format;

    use super::{ARCHIVE, WHOLE, named, one_name_many_times};

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
            assert!(blaze_link::fuzz_read(Format::Wasm, Arch::Wasm32, WHOLE, &bytes), "seed `{what}` is refused");
            let indexed = !what.contains("without an index");
            assert_eq!(
                blaze_link::fuzz_read(Format::Wasm, Arch::Wasm32, ARCHIVE, &bytes),
                indexed,
                "seed `{what}` without `whole`"
            );
        }
        assert!(blaze_link::fuzz_read(Format::Wasm, Arch::Wasm32, ARCHIVE, &one_name_many_times()));
    }
}
