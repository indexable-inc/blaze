//! Mach-O dynamic libraries (`MH_DYLIB`, `MH_DYLIB_STUB`, thin or in a universal file), read
//! as the linker reads one on its link line: `blaze_link::fuzz_read(Format::MachO, arch,
//! NativeKind::Shared { as_needed }, bytes)`, for x86-64 and AArch64. The install name comes
//! from `LC_ID_DYLIB`, the exports from the export trie (`LC_DYLD_INFO_ONLY` or
//! `LC_DYLD_EXPORTS_TRIE`) or else the symbol table.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - the read returns, accepting or refusing, and never panics (a read past the input is a
//!   slice index out of range in the plain test, and ASan's report in the fuzz build);
//! - the largest single allocation of a read is at most `blaze_fuzz::allowed(len)`;
//! - the bytes at an address 8 does not divide read as they do 8-aligned;
//! - `as_needed` changes nothing the read accepts;
//! - an accepted Mach-O or universal file (not a `.tbd`, whose text runs to the end) is still
//!   accepted with 64 bytes after its end: the trie, the tables and the slices are all named by
//!   an offset and a size.

#![forbid(unsafe_code)]

use std::time::Duration;

use blaze_codegen::Arch;
use blaze_fuzzer::Target;
use blaze_link::{Format, NativeKind};
use object::macho;

const TARGET: Target = Target {
    name: "link_macho_dylib",
    check,
    seeds,
    dictionary: &[
        b"\xcf\xfa\xed\xfe",
        b"\xca\xfe\xba\xbe",
        b"\x06\x00\x00\x00",
        b"\x09\x00\x00\x00",
        b"\x0d\x00\x00\x00",
        b"\x1f\x00\x00\x80",
        b"\x22\x00\x00\x80",
        b"\x33\x00\x00\x80",
        b"\x02\x00\x00\x00",
        b"\x19\x00\x00\x00",
        b"__TEXT\0\0\0\0\0\0\0\0\0\0",
        b"__thread_vars\0\0\0",
        b"/usr/lib/",
        b"\x00\x01_\x00",
        b"\x80\x80\x01",
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
    let binary = data.starts_with(&macho::MH_MAGIC_64.to_le_bytes())
        || data.starts_with(&macho::FAT_MAGIC.to_be_bytes())
        || data.starts_with(&macho::FAT_MAGIC_64.to_be_bytes());
    let bound = blaze_fuzz::allowed(data.len());
    let (largest, read) =
        blaze_fuzz::largest_allocation(|| blaze_link::fuzz_read(Format::MachO, arch, SHARED, aligned));
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
                blaze_link::fuzz_read(Format::MachO, arch, SHARED, shifted),
                read,
                "alignment: a {arch:?} read of the bytes one past an 8-aligned address answers otherwise"
            );
        }
        1 => {
            assert_eq!(
                blaze_link::fuzz_read(Format::MachO, arch, AS_NEEDED, aligned),
                read,
                "as needed: a {arch:?} read with `as_needed` answers otherwise"
            );
        }
        _ => {
            if read && binary {
                let (padded, at) = copy_at(data, 0, 64);
                let padded = &padded[at..][..data.len() + 64];
                assert!(
                    blaze_link::fuzz_read(Format::MachO, arch, SHARED, padded),
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
    let x86 = with_trie(macho::CPU_TYPE_X86_64.0, &sample_trie());
    let arm = with_trie(macho::CPU_TYPE_ARM64.0, &sample_trie());
    let both = fat(&[(macho::CPU_TYPE_X86_64.0, 3, &x86[..]), (macho::CPU_TYPE_ARM64.0, 0, &arm[..])]);
    vec![
        ("an AArch64 dylib with an LC_DYLD_INFO_ONLY trie", Arch::Aarch64, arm),
        ("an x86-64 dylib with an LC_DYLD_INFO_ONLY trie", Arch::X86_64, x86),
        ("an x86-64 dylib with an LC_DYLD_EXPORTS_TRIE", Arch::X86_64, exports_trie(macho::CPU_TYPE_X86_64.0)),
        ("an AArch64 dylib of a symbol table", Arch::Aarch64, symtab(macho::CPU_TYPE_ARM64.0, macho::MH_DYLIB.0)),
        ("an x86-64 dylib stub of a symbol table", Arch::X86_64, symtab(macho::CPU_TYPE_X86_64.0, macho::MH_DYLIB_STUB.0)),
        ("a universal dylib, its x86-64 slice", Arch::X86_64, both.clone()),
        ("a universal dylib, its AArch64 slice", Arch::Aarch64, both),
    ]
}

fn u32le(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn u64le(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn name16(out: &mut Vec<u8>, name: &str) {
    let mut field = [0u8; 16];
    field[..name.len()].copy_from_slice(name.as_bytes());
    out.extend_from_slice(&field);
}

fn segment(name: &str, vmaddr: u64, fileoff: u64, filesize: u64, sections: &[(&str, u64, u64, u32)]) -> Vec<u8> {
    let mut out = Vec::new();
    u32le(&mut out, macho::LC_SEGMENT_64.0);
    u32le(&mut out, 72 + 80 * sections.len() as u32);
    name16(&mut out, name);
    u64le(&mut out, vmaddr);
    u64le(&mut out, 0x4000);
    u64le(&mut out, fileoff);
    u64le(&mut out, filesize);
    u32le(&mut out, 7);
    u32le(&mut out, 5);
    u32le(&mut out, sections.len() as u32);
    u32le(&mut out, 0);
    for &(sect, addr, size, flags) in sections {
        name16(&mut out, sect);
        name16(&mut out, name);
        u64le(&mut out, addr);
        u64le(&mut out, size);
        for value in [0, 4, 0, 0, flags, 0, 0, 0] {
            u32le(&mut out, value);
        }
    }
    out
}

fn dylib_command(cmd: u32, path: &str) -> Vec<u8> {
    let size = (24 + path.len() + 1).next_multiple_of(8);
    let mut out = Vec::new();
    u32le(&mut out, cmd);
    u32le(&mut out, size as u32);
    for value in [24, 2, 0x10000, 0x10000] {
        u32le(&mut out, value);
    }
    out.extend_from_slice(path.as_bytes());
    out.resize(size, 0);
    out
}

fn dyld_info(export_off: u32, export_size: u32) -> Vec<u8> {
    let mut out = Vec::new();
    u32le(&mut out, macho::LC_DYLD_INFO_ONLY.0);
    u32le(&mut out, 48);
    for _ in 0..8 {
        u32le(&mut out, 0);
    }
    u32le(&mut out, export_off);
    u32le(&mut out, export_size);
    out
}

/// A dylib of `cpu` and `filetype` whose commands `commands(tail)` make, given where `tail`
/// (the linkedit data) starts in the file.
fn file(cpu: u32, filetype: u32, commands: impl Fn(u32) -> Vec<Vec<u8>>, tail: &[u8]) -> Vec<u8> {
    let sizeofcmds: usize = commands(0).iter().map(Vec::len).sum();
    let offset = (32 + sizeofcmds).next_multiple_of(8);
    let commands = commands(offset as u32);
    let mut out = Vec::new();
    for value in [macho::MH_MAGIC_64, cpu, 0, filetype, commands.len() as u32, sizeofcmds as u32, 0, 0] {
        u32le(&mut out, value);
    }
    for command in commands {
        out.extend_from_slice(&command);
    }
    out.resize(offset, 0);
    out.extend_from_slice(tail);
    out
}

/// `__TEXT` (mapping the header, `__text` code at 0x1000) and `__DATA` (`__data` at 0x4000,
/// `__thread_vars` at 0x4100).
fn segments(len: u64) -> Vec<Vec<u8>> {
    let code = macho::S_ATTR_PURE_INSTRUCTIONS.0 | macho::S_ATTR_SOME_INSTRUCTIONS.0;
    vec![
        segment("__TEXT", 0, 0, len, &[("__text", 0x1000, 0x100, code)]),
        segment(
            "__DATA",
            0x4000,
            0x4000,
            0,
            &[("__data", 0x4000, 0x100, 0), ("__thread_vars", 0x4100, 0x30, macho::S_THREAD_LOCAL_VARIABLES.to_flags().0)],
        ),
    ]
}

fn with_trie(cpu: u32, trie: &[u8]) -> Vec<u8> {
    let size = trie.len() as u32;
    file(
        cpu,
        macho::MH_DYLIB.0,
        |tail| {
            let mut commands = segments(0x1000);
            commands.push(dylib_command(macho::LC_ID_DYLIB.0, "/usr/lib/libfoo.dylib"));
            commands.push(dylib_command(macho::LC_REEXPORT_DYLIB.0, "/usr/lib/libbar.dylib"));
            commands.push(dyld_info(tail, size));
            commands
        },
        trie,
    )
}

fn exports_trie(cpu: u32) -> Vec<u8> {
    let trie = sample_trie();
    let size = trie.len() as u32;
    file(
        cpu,
        macho::MH_DYLIB.0,
        |tail| {
            let mut commands = segments(0x1000);
            commands.push(dylib_command(macho::LC_ID_DYLIB.0, "/usr/lib/libfoo.dylib"));
            let mut command = Vec::new();
            for value in [macho::LC_DYLD_EXPORTS_TRIE.0, 16, tail, size] {
                u32le(&mut command, value);
            }
            commands.push(command);
            commands
        },
        &trie,
    )
}

/// A library without a trie: its exports are the symbol table's external definitions (and
/// not its local, private-external or undefined symbols).
fn symtab(cpu: u32, filetype: u32) -> Vec<u8> {
    let symbols: [(&str, u8, u8, u16, u64); 8] = [
        ("_f", macho::N_SECT.0 | macho::N_EXT.0, 1, 0, 0x1000),
        ("_d", macho::N_SECT.0 | macho::N_EXT.0, 2, macho::N_WEAK_DEF.0, 0x4000),
        ("_t", macho::N_SECT.0 | macho::N_EXT.0, 3, 0, 0x4100),
        ("_local", macho::N_SECT.0, 1, 0, 0x1004),
        ("_pext", macho::N_SECT.0 | macho::N_EXT.0 | macho::N_PEXT.0, 1, 0, 0x1008),
        ("_undef", macho::N_UNDF.0 | macho::N_EXT.0, 0, 0, 0),
        ("_abs", macho::N_ABS.0 | macho::N_EXT.0, 0, 0, 0x42),
        ("_indirect", macho::N_INDR.0 | macho::N_EXT.0, 0, 0, 1),
    ];
    let mut strings = vec![0u8];
    let mut table = Vec::new();
    for (name, n_type, n_sect, n_desc, value) in symbols {
        u32le(&mut table, strings.len() as u32);
        table.extend_from_slice(&[n_type, n_sect]);
        table.extend_from_slice(&n_desc.to_le_bytes());
        u64le(&mut table, value);
        strings.extend_from_slice(name.as_bytes());
        strings.push(0);
    }
    let nsyms = symbols.len() as u32;
    let strsize = strings.len() as u32;
    let table_len = table.len() as u32;
    let mut tail = table;
    tail.extend_from_slice(&strings);
    file(
        cpu,
        filetype,
        |at| {
            let mut commands = segments(0x1000);
            commands.push(dylib_command(macho::LC_ID_DYLIB.0, "/usr/lib/libsym.dylib"));
            let mut command = Vec::new();
            for value in [macho::LC_SYMTAB.0, 24, at, nsyms, at + table_len, strsize] {
                u32le(&mut command, value);
            }
            commands.push(command);
            commands
        },
        &tail,
    )
}

fn encode_uleb(out: &mut Vec<u8>, mut value: u64) {
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

fn uleb_len(value: u64) -> usize {
    let mut out = Vec::new();
    encode_uleb(&mut out, value);
    out.len()
}

struct Node {
    terminal: Vec<u8>,
    children: Vec<(&'static str, Node)>,
}

fn leaf(terminal: Vec<u8>) -> Node {
    Node { terminal, children: Vec::new() }
}

fn flatten<'a>(node: &'a Node, out: &mut Vec<(&'a Node, Vec<(&'static str, usize)>)>) -> usize {
    let index = out.len();
    out.push((node, Vec::new()));
    for &(label, ref child) in &node.children {
        let child = flatten(child, out);
        out[index].1.push((label, child));
    }
    index
}

/// Lays a trie out as ld64 does: offsets grown until every child offset's ULEB fits.
fn build_trie(root: &Node) -> Vec<u8> {
    let mut nodes = Vec::new();
    flatten(root, &mut nodes);
    let mut offsets = vec![0u64; nodes.len()];
    loop {
        let mut next = Vec::with_capacity(nodes.len());
        let mut at = 0u64;
        for (node, children) in &nodes {
            next.push(at);
            let len = node.terminal.len() as u64;
            at += uleb_len(len) as u64 + len + 1;
            for (label, child) in children {
                at += label.len() as u64 + 1 + uleb_len(offsets[*child]) as u64;
            }
        }
        if next == offsets {
            break;
        }
        offsets = next;
    }
    let mut out = Vec::new();
    for (index, (node, children)) in nodes.iter().enumerate() {
        assert_eq!(out.len() as u64, offsets[index], "the trie's offset of node {index}");
        encode_uleb(&mut out, node.terminal.len() as u64);
        out.extend_from_slice(&node.terminal);
        out.push(children.len() as u8);
        for (label, child) in children {
            out.extend_from_slice(label.as_bytes());
            out.push(0);
            encode_uleb(&mut out, offsets[*child]);
        }
    }
    out
}

fn info(flags: u64, values: &[u64]) -> Vec<u8> {
    let mut out = Vec::new();
    encode_uleb(&mut out, flags);
    for &value in values {
        encode_uleb(&mut out, value);
    }
    out
}

fn reexport(flags: u64, ordinal: u64, import: &str) -> Vec<u8> {
    let mut out = info(flags | macho::EXPORT_SYMBOL_FLAGS_REEXPORT.0, &[ordinal]);
    out.extend_from_slice(import.as_bytes());
    out.push(0);
    out
}

/// A trie of every kind of export: regular in code and data, weak, re-exported (renamed and
/// not), thread-local, absolute, stub-and-resolver.
fn sample_trie() -> Vec<u8> {
    let foo = Node {
        terminal: info(0, &[0x1010]),
        children: vec![("bar", leaf(info(macho::EXPORT_SYMBOL_FLAGS_WEAK_DEFINITION.0, &[0x4010])))],
    };
    let fo = Node {
        terminal: Vec::new(),
        children: vec![
            ("o", foo),
            ("b", leaf(reexport(0, 1, "_bob"))),
            ("_tls", leaf(info(u64::from(macho::EXPORT_SYMBOL_FLAGS_KIND_THREAD_LOCAL.0), &[0x4100]))),
        ],
    };
    let underscore = Node {
        terminal: Vec::new(),
        children: vec![
            ("fo", fo),
            ("abs", leaf(info(u64::from(macho::EXPORT_SYMBOL_FLAGS_KIND_ABSOLUTE.0), &[0x1234]))),
            ("res", leaf(info(macho::EXPORT_SYMBOL_FLAGS_STUB_AND_RESOLVER.0, &[0x1020, 0x1030]))),
            ("same", leaf(reexport(0, 1, ""))),
        ],
    };
    build_trie(&Node { terminal: Vec::new(), children: vec![("_", underscore)] })
}

/// A universal (`FAT_MAGIC`) file of `slices` (CPU type, subtype, thin file), each 8-aligned.
fn fat(slices: &[(u32, u32, &[u8])]) -> Vec<u8> {
    let mut at = (8 + 20 * slices.len()).next_multiple_of(8);
    let mut out = Vec::new();
    out.extend_from_slice(&macho::FAT_MAGIC.to_be_bytes());
    out.extend_from_slice(&(slices.len() as u32).to_be_bytes());
    for (cpu, subtype, bytes) in slices {
        for value in [*cpu, *subtype, at as u32, bytes.len() as u32, 3] {
            out.extend_from_slice(&value.to_be_bytes());
        }
        at = (at + bytes.len()).next_multiple_of(8);
    }
    for (_, _, bytes) in slices {
        out.resize(out.len().next_multiple_of(8), 0);
        out.extend_from_slice(bytes);
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
            assert!(blaze_link::fuzz_read(Format::MachO, arch, SHARED, &bytes), "seed `{what}` is refused");
        }
    }
}
