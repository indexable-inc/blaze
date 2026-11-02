//! A Mach-O dylib's export trie alone: the input is the trie, placed in a valid dylib (its
//! `__TEXT` mapping the header, `__text` code at 0x1000, `__data` and `__thread_vars` at 0x4000,
//! an `LC_ID_DYLIB`) and read as the linker reads the library:
//! `blaze_link::fuzz_read(Format::MachO, arch, NativeKind::Shared { as_needed: false }, dylib)`.
//! Every mutation lands in the trie, the part `link_macho_dylib`'s whole-file mutations rarely
//! reach whole.
//!
//! Invariants, each checked below (a violation panics, which the engine reports as a crash):
//! - the read returns, accepting or refusing, and never panics or hangs (a cycle is an error);
//! - the trie walk and the reference walk `dylib::tests` keeps agree, on the same exports or the
//!   same error (`blaze_link::fuzz_trie_differential`);
//! - the largest single allocation of a read is at most `blaze_fuzz::allowed(len)` of the
//!   trie's length (the dylib around it is under 400 bytes);
//! - the trie reads alike from `LC_DYLD_INFO_ONLY` and `LC_DYLD_EXPORTS_TRIE`, for x86-64 and
//!   AArch64: nothing but the trie's bytes differs between the four dylibs;
//! - a dylib whose trie is at an address 8 does not divide reads as it does 8-aligned.

#![forbid(unsafe_code)]

use std::time::Duration;

use blaze_codegen::Arch;
use blaze_fuzzer::Target;
use blaze_link::{Format, NativeKind};
use object::macho;

const TARGET: Target = Target {
    name: "link_macho_trie",
    check,
    seeds,
    dictionary: &[
        b"\x00\x01_\x00",
        b"\x00\x00",
        b"\x80\x80\x01",
        b"\xff\xff\xff\xff\xff\xff\xff\xff\xff\x01",
        b"\x08",
        b"\x04",
        b"\x10",
        b"\x01",
        b"\x02",
        b"\x20",
        b"\x90\x20",
        b"\x90\x80\x01",
        b"_foo\x00",
    ],
    max_len: 1 << 16,
    budget: Duration::from_secs(600),
};

const SHARED: NativeKind = NativeKind::Shared { as_needed: false };

/// Where a dylib holds the trie: its arch, its CPU and the command.
const HOLDERS: [(Arch, u32, u32); 4] = [
    (Arch::Aarch64, macho::CPU_TYPE_ARM64.0, macho::LC_DYLD_INFO_ONLY.0),
    (Arch::Aarch64, macho::CPU_TYPE_ARM64.0, macho::LC_DYLD_EXPORTS_TRIE.0),
    (Arch::X86_64, macho::CPU_TYPE_X86_64.0, macho::LC_DYLD_INFO_ONLY.0),
    (Arch::X86_64, macho::CPU_TYPE_X86_64.0, macho::LC_DYLD_EXPORTS_TRIE.0),
];

/// The holder read against the first, one a run (`blaze_fuzzer::case`); the plain test takes
/// every one of each seed.
const CASES: usize = HOLDERS.len() - 1;

fn check(trie: &[u8]) {
    check_case(trie, blaze_fuzzer::case(trie, CASES));
}

fn check_case(trie: &[u8], case: usize) {
    if let Err(e) = blaze_link::fuzz_trie_differential(trie) {
        panic!("differential: {e}");
    }
    let bound = blaze_fuzz::allowed(trie.len());
    let mut first = None;
    for (arch, cpu, command) in [HOLDERS[0], HOLDERS[1 + case]] {
        let dylib = with_trie(cpu, command, trie);
        let (aligned, at) = copy_at(&dylib, 0);
        let aligned = &aligned[at..][..dylib.len()];
        let (largest, read) =
            blaze_fuzz::largest_allocation(|| blaze_link::fuzz_read(Format::MachO, arch, SHARED, aligned));
        assert!(
            largest <= bound,
            "allocation bound: a {arch:?} read of a {}-byte trie asked for {largest} bytes at once, past {bound}",
            trie.len()
        );
        match first {
            None => first = Some(read),
            Some(first) => assert_eq!(
                read, first,
                "same trie: the trie read from command {command:#x} for {arch:?} answers otherwise"
            ),
        }
        if command == macho::LC_DYLD_EXPORTS_TRIE.0 {
            let (shifted, at) = copy_at(&dylib, 1);
            let shifted = &shifted[at..][..dylib.len()];
            assert_eq!(
                blaze_link::fuzz_read(Format::MachO, arch, SHARED, shifted),
                read,
                "alignment: a {arch:?} read of the dylib one past an 8-aligned address answers otherwise"
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

/// `sample_trie` and tries made as `dylib::tests::random_node` makes them (the seeds of
/// `trie_walk_matches_reference`).
fn seeds() -> Vec<Vec<u8>> {
    let mut rng = Rng(0xd1_1b);
    let mut out = vec![sample_trie(), Vec::new()];
    for _ in 0..64 {
        out.push(build_trie(&random_node(&mut rng, 0)));
    }
    out
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

/// A dylib of `cpu` whose export trie, `trie` after the load commands, `command` names
/// (`LC_DYLD_INFO_ONLY` or `LC_DYLD_EXPORTS_TRIE`).
fn with_trie(cpu: u32, command: u32, trie: &[u8]) -> Vec<u8> {
    let code = macho::S_ATTR_PURE_INSTRUCTIONS.0 | macho::S_ATTR_SOME_INSTRUCTIONS.0;
    let mut commands = vec![
        segment("__TEXT", 0, 0, 0x1000, &[("__text", 0x1000, 0x100, code)]),
        segment(
            "__DATA",
            0x4000,
            0x4000,
            0,
            &[("__data", 0x4000, 0x100, 0), ("__thread_vars", 0x4100, 0x30, macho::S_THREAD_LOCAL_VARIABLES.to_flags().0)],
        ),
    ];
    let path = "/usr/lib/libfoo.dylib";
    let mut id = Vec::new();
    let size = (24 + path.len() + 1).next_multiple_of(8);
    for value in [macho::LC_ID_DYLIB.0, size as u32, 24, 2, 0x10000, 0x10000] {
        u32le(&mut id, value);
    }
    id.extend_from_slice(path.as_bytes());
    id.resize(size, 0);
    commands.push(id);
    let trie_command_len = if command == macho::LC_DYLD_INFO_ONLY.0 { 48 } else { 16 };
    let sizeofcmds = commands.iter().map(Vec::len).sum::<usize>() + trie_command_len;
    let offset = (32 + sizeofcmds).next_multiple_of(8) as u32;
    let mut trie_command = Vec::new();
    u32le(&mut trie_command, command);
    u32le(&mut trie_command, trie_command_len as u32);
    if command == macho::LC_DYLD_INFO_ONLY.0 {
        for _ in 0..8 {
            u32le(&mut trie_command, 0);
        }
    }
    u32le(&mut trie_command, offset);
    u32le(&mut trie_command, trie.len() as u32);
    commands.push(trie_command);

    let mut out = Vec::new();
    for value in [macho::MH_MAGIC_64, cpu, 0, macho::MH_DYLIB.0, commands.len() as u32, sizeofcmds as u32, 0, 0] {
        u32le(&mut out, value);
    }
    for command in commands {
        out.extend_from_slice(&command);
    }
    out.resize(offset as usize, 0);
    out.extend_from_slice(trie);
    out
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

/// `dylib::tests`' generator: splitmix64.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

const LABELS: [&str; 9] = ["_", "a", "b", "foo", "o", "bar", "_x", "\u{e9}", "longer_label"];

fn random_info(rng: &mut Rng) -> Vec<u8> {
    let addr = [0x1010, 0x4010, 0x4100, 0x9999][rng.below(4)];
    match rng.below(7) {
        0 => Vec::new(),
        1 => info(0, &[addr]),
        2 => info(macho::EXPORT_SYMBOL_FLAGS_WEAK_DEFINITION.0, &[addr]),
        3 => info(u64::from(macho::EXPORT_SYMBOL_FLAGS_KIND_THREAD_LOCAL.0), &[addr]),
        4 => info(u64::from(macho::EXPORT_SYMBOL_FLAGS_KIND_ABSOLUTE.0), &[addr]),
        5 => info(macho::EXPORT_SYMBOL_FLAGS_STUB_AND_RESOLVER.0, &[addr, 0x1030]),
        _ => reexport(0, 1, "_other"),
    }
}

fn random_node(rng: &mut Rng, depth: usize) -> Node {
    let children = if depth > 4 { 0 } else { rng.below(4) };
    Node {
        terminal: random_info(rng),
        children: (0..children).map(|_| (LABELS[rng.below(LABELS.len())], random_node(rng, depth + 1))).collect(),
    }
}

#[cfg_attr(test, allow(dead_code))]
fn main() -> std::process::ExitCode {
    blaze_fuzzer::main(&TARGET)
}

#[cfg(test)]
mod tests {
    use blaze_codegen::Arch;
    use blaze_link::Format;
    use object::macho;

    use super::{SHARED, sample_trie, with_trie};

    #[test]
    fn seeds() {
        blaze_fuzzer::seeds(&super::TARGET);
    }

    #[test]
    fn every_case() {
        blaze_fuzzer::every_case(&super::TARGET, super::CASES, super::check_case);
    }

    #[test]
    fn sample_trie_reads() {
        for command in [macho::LC_DYLD_INFO_ONLY.0, macho::LC_DYLD_EXPORTS_TRIE.0] {
            let dylib = with_trie(macho::CPU_TYPE_ARM64.0, command, &sample_trie());
            assert!(blaze_link::fuzz_read(Format::MachO, Arch::Aarch64, SHARED, &dylib), "command {command:#x}");
        }
        assert_eq!(blaze_link::fuzz_trie_differential(&sample_trie()), Ok(true));
    }
}
