//! Images written from hand-made `Placed` values and read back with `object`'s Mach-O reader
//! and decoders of the opcodes, the trie and the signature written here.

use blaze_codegen::Arch;
use object::LittleEndian as LE;
use object::macho;
use object::read::macho::{MachHeader as _, Section as _, Segment as _};

use super::{core_name, linkedit, macho_name, sha256, sign};
use crate::model::{
    Bind, Class, Delta, Export, Function, GotEntry, Holds, Import, Library, Placed, Plan, Prot,
    Region, Reserve, SectionKind, Special, Splice, SymbolKind, Tls, Writer,
};
use crate::output::{Debug, Platform};
use crate::{DevRun, Format, Output, OutputKind};

/// macOS 11: the opcodes' deployment target, before chained fixups (12).
const OPCODES: u32 = 0x000b_0000;
const CHAINED: u32 = 0x000c_0000;

fn output(arch: Arch, kind: OutputKind) -> Output {
    output_at(arch, kind, OPCODES)
}

fn output_at(arch: Arch, kind: OutputKind, minimum: u32) -> Output {
    Output {
        format: Format::MachO,
        arch,
        kind,
        interpreter: None,
        platform: Some(Platform { platform: 1, minimum, sdk: 0x000f_0000 }),
        incremental: false,
        debug: Debug::Lines,
        live: None,
        tags: 0,
        stack: None,
        windows_subsystem: false,
        dev_run: None,
        exec_log: false,
        sanitize: blaze_codegen::settings::Sanitize::default(),
        rpaths: Vec::new(),
        triple: match arch { Arch::X86_64 => "x86_64-apple-darwin", _ => "aarch64-apple-darwin" }.into(),
    }
}

fn executable(arch: Arch) -> Output {
    output(arch, OutputKind::Executable { entry: "main".to_owned() })
}

fn region(class: Class, addr: u64, size: u64, capacity: u64, prot: Prot) -> Region {
    Region { class, addr, capacity, size, prot, align: 8 }
}

/// The file as the core writes it: the regions at their offsets, the encoded slots over them
/// (`Writer::encode`, which `file` sees as `Placed::encoded`), then the writer's parts and
/// the core's debug sections where the writer says.
fn written(writer: &dyn Writer, placed: &Placed<'_>) -> Vec<u8> {
    let encoded = writer.encode(placed).expect("the slots");
    let placed = Placed { encoded: &encoded, ..*placed };
    let file = writer.file(&placed).expect("the file");
    let mut bytes = vec![0u8; file.len as usize];
    for (index, region) in placed.regions.iter().enumerate() {
        if let Some(at) = writer.offset(&placed, region) {
            let content = placed.bytes(index);
            bytes[at as usize..at as usize + content.len()].copy_from_slice(content);
        }
    }
    for &(addr, value) in &encoded {
        let at = (addr - placed.plan.base) as usize;
        bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
    }
    for (at, part) in &file.parts {
        part.copy_to(0, &mut bytes[*at as usize..*at as usize + part.len()]);
    }
    for &(at, nth) in &file.debug {
        let section = &placed.debug[nth].1;
        bytes[at as usize..at as usize + section.len()].copy_from_slice(section);
    }
    bytes
}

fn uleb(bytes: &[u8], at: &mut usize) -> u64 {
    blaze_format::leb::read_u64(bytes, at).expect("a ULEB128")
}

fn sleb(bytes: &[u8], at: &mut usize) -> i64 {
    blaze_format::leb::read_i64(bytes, at).expect("an SLEB128")
}

fn c_string(bytes: &[u8], at: &mut usize) -> Vec<u8> {
    let end = *at + bytes[*at..].iter().position(|it| *it == 0).unwrap();
    let name = bytes[*at..end].to_vec();
    *at = end + 1;
    name
}

/// What dyld does with rebase opcodes: the addresses it slides.
fn decode_rebases(stream: &[u8], segments: &[u64]) -> Vec<u64> {
    let (mut found, mut at, mut address) = (Vec::new(), 0, 0u64);
    while at < stream.len() {
        let byte = stream[at];
        at += 1;
        let immediate = u64::from(byte & macho::REBASE_IMMEDIATE_MASK);
        match macho::RebaseOpcode(byte & macho::REBASE_OPCODE_MASK) {
            macho::REBASE_OPCODE_DONE => break,
            macho::REBASE_OPCODE_SET_TYPE_IMM => {
                assert_eq!(immediate, u64::from(macho::REBASE_TYPE_POINTER.0))
            }
            macho::REBASE_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB => {
                address = segments[immediate as usize] + uleb(stream, &mut at);
            }
            macho::REBASE_OPCODE_ADD_ADDR_ULEB => address += uleb(stream, &mut at),
            macho::REBASE_OPCODE_ADD_ADDR_IMM_SCALED => address += immediate * 8,
            macho::REBASE_OPCODE_DO_REBASE_IMM_TIMES => {
                for _ in 0..immediate {
                    found.push(address);
                    address += 8;
                }
            }
            macho::REBASE_OPCODE_DO_REBASE_ULEB_TIMES => {
                for _ in 0..uleb(stream, &mut at) {
                    found.push(address);
                    address += 8;
                }
            }
            other => panic!("rebase opcode {other:#x}"),
        }
    }
    found
}

/// What dyld does with bind opcodes: (address, ordinal, name, flags, addend).
fn decode_binds(stream: &[u8], segments: &[u64]) -> Vec<(u64, i64, Vec<u8>, u8, i64)> {
    let (mut found, mut at, mut address) = (Vec::new(), 0, 0u64);
    let (mut ordinal, mut name, mut flags, mut addend) = (0i64, Vec::new(), 0u8, 0i64);
    while at < stream.len() {
        let byte = stream[at];
        at += 1;
        let immediate = byte & macho::BIND_IMMEDIATE_MASK;
        match macho::BindOpcode(byte & macho::BIND_OPCODE_MASK) {
            macho::BIND_OPCODE_DONE => break,
            macho::BIND_OPCODE_SET_DYLIB_ORDINAL_IMM => ordinal = i64::from(immediate),
            macho::BIND_OPCODE_SET_DYLIB_ORDINAL_ULEB => ordinal = uleb(stream, &mut at) as i64,
            macho::BIND_OPCODE_SET_DYLIB_SPECIAL_IMM => {
                ordinal = if immediate == 0 { 0 } else { i64::from((immediate | 0xf0) as i8) };
            }
            macho::BIND_OPCODE_SET_SYMBOL_TRAILING_FLAGS_IMM => {
                flags = immediate;
                name = c_string(stream, &mut at);
            }
            macho::BIND_OPCODE_SET_TYPE_IMM => assert_eq!(immediate, macho::BIND_TYPE_POINTER.0),
            macho::BIND_OPCODE_SET_ADDEND_SLEB => addend = sleb(stream, &mut at),
            macho::BIND_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB => {
                address = segments[immediate as usize] + uleb(stream, &mut at);
            }
            macho::BIND_OPCODE_ADD_ADDR_ULEB => address += uleb(stream, &mut at),
            macho::BIND_OPCODE_DO_BIND => {
                found.push((address, ordinal, name.clone(), flags, addend));
                address += 8;
            }
            other => panic!("bind opcode {other:#x}"),
        }
    }
    found.sort();
    found
}

/// Every export of a trie: (name, flags, address).
fn decode_trie(trie: &[u8]) -> Vec<(Vec<u8>, u64, u64)> {
    let mut found = Vec::new();
    let mut stack = vec![(0usize, Vec::new())];
    let mut seen = 0;
    while let Some((node, prefix)) = stack.pop() {
        seen += 1;
        assert!(seen < 10_000, "a cycle in the trie");
        let mut at = node;
        let info = uleb(trie, &mut at) as usize;
        let children_at = at + info;
        if info != 0 {
            let flags = uleb(trie, &mut at);
            let address = uleb(trie, &mut at);
            found.push((prefix.clone(), flags, address));
        }
        at = children_at;
        let children = trie[at];
        at += 1;
        for _ in 0..children {
            let label = c_string(trie, &mut at);
            let child = uleb(trie, &mut at) as usize;
            let mut name = prefix.clone();
            name.extend_from_slice(&label);
            stack.push((child, name));
        }
    }
    found.sort();
    found
}

/// A trie lookup as dyld does it, by walking edges.
fn trie_lookup(trie: &[u8], name: &[u8]) -> Option<u64> {
    let (mut node, mut rest) = (0usize, name);
    loop {
        let mut at = node;
        let info = uleb(trie, &mut at) as usize;
        if rest.is_empty() {
            if info == 0 {
                return None;
            }
            uleb(trie, &mut at);
            return Some(uleb(trie, &mut at));
        }
        at += info;
        let children = trie[at];
        at += 1;
        let mut next = None;
        for _ in 0..children {
            let label = c_string(trie, &mut at);
            let child = uleb(trie, &mut at) as usize;
            if rest.starts_with(&label) && next.is_none() {
                next = Some((child, label.len()));
            }
        }
        let (child, len) = next?;
        node = child;
        rest = &rest[len..];
    }
}

fn spans() -> Vec<linkedit::Span> {
    vec![
        linkedit::Span { name: "__PAGEZERO", start: 0, end: 0x1_0000_0000, writable: false },
        linkedit::Span {
            name: "__TEXT",
            start: 0x1_0000_0000,
            end: 0x1_0000_8000,
            writable: false,
        },
        linkedit::Span {
            name: "__DATA",
            start: 0x1_0000_8000,
            end: 0x1_0001_0000,
            writable: true,
        },
    ]
}

/// What an encoder appends to a buffer already holding 3 bytes (so off 8-byte alignment): the
/// table alone, checked to be the length it says, padded to 8 bytes from where it starts, and
/// to leave the bytes before it alone.
fn appended(encode: impl FnOnce(&mut Vec<u8>) -> Result<usize, String>) -> Result<Vec<u8>, String> {
    let mut out = vec![0xaa; 3];
    let len = encode(&mut out)?;
    assert_eq!(out.len(), 3 + len, "the length it says");
    assert_eq!(len % 8, 0, "padded to 8 bytes");
    assert_eq!(out[..3], [0xaa; 3], "what was before it");
    Ok(out.split_off(3))
}

fn encode_rebases(addrs: &[u64], spans: &[linkedit::Span]) -> Result<Vec<u8>, String> {
    appended(|out| linkedit::rebases(addrs, spans, out))
}

fn encode_binds(binds: &mut [linkedit::Bind<'_>], spans: &[linkedit::Span]) -> Result<Vec<u8>, String> {
    appended(|out| linkedit::binds(binds, spans, out))
}

fn encode_trie<N: AsRef<[u8]> + Ord>(exports: &mut [(N, u64, u64)]) -> Result<Vec<u8>, String> {
    appended(|out| linkedit::trie(exports, out))
}

fn encode_starts(text: u64, addrs: &[u64]) -> Vec<u8> {
    appended(|out| Ok(linkedit::function_starts(text, addrs, out))).unwrap()
}

#[test]
fn names_cross_the_edge_both_ways() {
    assert_eq!(core_name(b"_main").unwrap(), "main");
    assert_eq!(core_name(b"__mh_execute_header").unwrap(), "_mh_execute_header");
    assert_eq!(core_name(b"section$start$__DATA$__x").unwrap(), "\x01section$start$__DATA$__x");
    for name in ["main", "_mh_execute_header", "\x01section$end$__TEXT$__text", "_ZN3foo3barE"] {
        assert_eq!(core_name(macho_name(name).as_bytes()).unwrap(), name);
    }
    assert!(core_name(&[b'_', 0xff]).is_err());
    let mut names = crate::model::Names::default();
    let (fits, long) = ("l".repeat(255), "l".repeat(256));
    for raw in [&b"_main"[..], b"__mh_execute_header", b"section$start$__DATA$__x", b"_", b"x", b"", fits.as_bytes(), long.as_bytes()] {
        let name = super::push_core_name(&mut names, raw).unwrap();
        assert_eq!(names.at(name), core_name(raw).unwrap());
    }
    assert!(super::push_core_name(&mut names, &[b'_', 0xff]).is_err());
}

#[test]
fn trie_finds_every_export() {
    let names: Vec<String> = ["_a", "_ab", "_abc", "_abd", "_b", "__mh_execute_header", "_main"]
        .iter()
        .map(|it| it.to_string())
        .chain((0..300).map(|it| format!("_sym{it}_{}", it * 7919 % 1000)))
        .collect();
    let mut exports: Vec<(Vec<u8>, u64, u64)> = (names.iter().enumerate())
        .map(|(nth, it)| (it.clone().into_bytes(), 0, 0x4000 + nth as u64 * 0x1234))
        .collect();
    let want = {
        let mut want = exports.clone();
        want.sort();
        want
    };
    let trie = encode_trie(&mut exports).unwrap();
    assert_eq!(trie.len() % 8, 0);
    assert_eq!(decode_trie(&trie), want);
    for (name, _, addr) in &want {
        assert_eq!(trie_lookup(&trie, name), Some(*addr), "{}", String::from_utf8_lossy(name));
    }
    assert_eq!(trie_lookup(&trie, b"_ac"), None);
    assert_eq!(trie_lookup(&trie, b"_"), None);
    let mut twice = vec![(b"_a".to_vec(), 0, 1), (b"_a".to_vec(), 0, 2)];
    assert!(encode_trie(&mut twice).is_err());
    assert!(encode_trie(&mut Vec::<(Vec<u8>, u64, u64)>::new()).unwrap().is_empty());
}

#[test]
fn rebases_in_runs() {
    let addrs = [
        0x1_0000_8000,
        0x1_0000_8008,
        0x1_0000_8010,
        0x1_0000_9000,
        0x1_0000_a000,
        0x1_0000_a008,
    ];
    let mut many: Vec<u64> = (0..40).map(|it| 0x1_0000_c000 + it * 8).collect();
    many.extend(addrs);
    let stream = encode_rebases(&many, &spans()).unwrap();
    let starts: Vec<u64> = spans().iter().map(|it| it.start).collect();
    let mut want = many.clone();
    want.sort();
    assert_eq!(decode_rebases(&stream, &starts), want);
    let text = encode_rebases(&[0x1_0000_4000], &spans()).unwrap_err();
    assert!(text.contains("__TEXT"), "{text}");
    assert!(encode_rebases(&[0x1_0000_8000, 0x1_0000_8000], &spans()).is_err());
    assert!(encode_rebases(&[0x2_0000_0000], &spans()).is_err());
}

#[test]
fn binds_decode_to_the_imports() {
    let bind = |addr, ordinal, name: &'static str, weak, addend| linkedit::Bind {
        addr,
        ordinal,
        name: name.as_bytes(),
        weak,
        addend,
    };
    let mut binds = vec![
        bind(0x1_0000_8000, 1, "_malloc", false, 0),
        bind(0x1_0000_8008, 1, "_free", false, 0),
        bind(0x1_0000_8010, 20, "_far", false, 0),
        bind(0x1_0000_8018, -2, "__tlv_bootstrap", false, 0),
        bind(0x1_0000_8020, -2, "_maybe", true, 0),
        bind(0x1_0000_8028, 1, "_malloc", false, 16),
        bind(0x1_0000_8030, 1, "_malloc", false, -8),
        bind(0x1_0000_9000, 1, "_malloc", false, 0),
    ];
    let mut want: Vec<(u64, i64, Vec<u8>, u8, i64)> = (binds.iter())
        .map(|it| {
            let flags = if it.weak { macho::BIND_SYMBOL_FLAGS_WEAK_IMPORT.0 } else { 0 };
            (it.addr, it.ordinal, it.name.to_vec(), flags, it.addend)
        })
        .collect();
    want.sort();
    let stream = encode_binds(&mut binds, &spans()).unwrap();
    let starts: Vec<u64> = spans().iter().map(|it| it.start).collect();
    assert_eq!(decode_binds(&stream, &starts), want);
    let mut text = vec![bind(0x1_0000_4000, 1, "_x", false, 0)];
    assert!(encode_binds(&mut text, &spans()).is_err());
    let mut twice =
        vec![bind(0x1_0000_8000, 1, "_x", false, 0), bind(0x1_0000_8000, 2, "_y", false, 0)];
    assert!(encode_binds(&mut twice, &spans()).is_err());
}

#[test]
fn function_starts_are_distances() {
    let starts = encode_starts(0x1_0000_0000, &[0x1_0000_4010, 0x1_0000_4000, 0x1_0000_4400]);
    let mut at = 0;
    assert_eq!(uleb(&starts, &mut at), 0x4000);
    assert_eq!(uleb(&starts, &mut at), 0x10);
    assert_eq!(uleb(&starts, &mut at), 0x3f0);
    assert_eq!(uleb(&starts, &mut at), 0);
    assert!(encode_starts(0x1_0000_0000, &[]).is_empty());
}

#[test]
fn signature_hashes_every_page() {
    let file: Vec<u8> = (0..3 * sign::PAGE + 100).map(|it| (it * 31 % 251) as u8).collect();
    let mut signature = Vec::new();
    sign::signature(&sign::View::whole(&file), "hello", 0, 0x4000, true, None, &mut signature);
    assert_eq!(signature.len() as u64, sign::len("hello", file.len() as u64));
    check_signature(&signature, &file, "hello");
}

/// The signature says the hash of every page of `file` and the identifier.
fn check_signature(signature: &[u8], file: &[u8], identifier: &str) {
    let be = |at: usize| u32::from_be_bytes(signature[at..at + 4].try_into().unwrap());
    assert_eq!(be(0), 0xfade_0cc0);
    assert_eq!(be(8), 1);
    assert_eq!(be(12), 0);
    let directory = be(16) as usize;
    let cd = |at: usize| be(directory + at);
    assert_eq!(cd(0), 0xfade_0c02);
    assert!(cd(8) >= 0x0002_0400);
    assert_eq!(cd(12) & 0x2, 0x2, "ad hoc");
    let (hashes, ident, special, slots, limit) =
        (cd(16) as usize, cd(20) as usize, cd(24), cd(28) as usize, cd(32) as usize);
    assert_eq!(special, 0);
    assert_eq!(limit, file.len());
    assert_eq!(&signature[directory + 36..directory + 40], &[32, 2, 0, 12]);
    let name = &signature[directory + ident..];
    let name = &name[..name.iter().position(|it| *it == 0).unwrap()];
    assert_eq!(name, identifier.as_bytes());
    assert_eq!(slots, file.len().div_ceil(4096));
    for (nth, page) in file.chunks(4096).enumerate() {
        let at = directory + hashes + nth * 32;
        assert_eq!(&signature[at..at + 32], &sha256::digest(page), "page {nth}");
    }
}

#[test]
fn wasm_and_no_platform_are_errors() {
    assert!(super::writer(&executable(Arch::Wasm32)).is_err());
    let mut output = executable(Arch::Aarch64);
    output.platform = None;
    assert!(super::writer(&output).is_err());
}

#[test]
fn plans() {
    let arm = super::writer(&executable(Arch::Aarch64)).unwrap().plan();
    assert_eq!((arm.base, arm.page, arm.tls_descriptors), (0x1_0000_0000, 0x4000, true));
    let x86 = super::writer(&executable(Arch::X86_64)).unwrap().plan();
    assert_eq!((x86.base, x86.page), (0x1_0000_0000, 0x1000));
    let shared = OutputKind::Shared { name: "@rpath/libx.dylib".to_owned(), exports: Vec::new() };
    assert_eq!(super::writer(&output(Arch::Aarch64, shared)).unwrap().plan().base, 0);
}

#[test]
fn classes_in_ld64_order() {
    let writer = super::writer(&executable(Arch::Aarch64)).unwrap();
    let linkme = Class::Named { name: "__DATA,__linkme_X".into(), write: true, exec: false };
    let mut classes = vec![
        Class::Bss,
        Class::Got,
        linkme.clone(),
        Class::EhFrame,
        Class::InitArray,
        Class::Tbss,
        Class::Text,
        Class::Synthetic(super::UNWIND_INFO),
        Class::Rodata,
        Class::Data,
        Class::Tdata,
        Class::Plt,
        super::compact_unwind(),
        Class::Table,
        Class::Patch,
        Class::ThreadVars,
        Class::RelRo,
    ];
    writer.order(&mut classes);
    assert_eq!(
        classes,
        vec![
            Class::Text,
            Class::Patch,
            Class::Plt,
            Class::Rodata,
            Class::Synthetic(super::UNWIND_INFO),
            Class::EhFrame,
            Class::Got,
            Class::InitArray,
            Class::RelRo,
            super::compact_unwind(),
            Class::Table,
            Class::Data,
            linkme,
            Class::ThreadVars,
            Class::Tdata,
            Class::Tbss,
            Class::Bss,
        ]
    );
    assert_eq!(writer.prot(&Class::Rodata), Prot::RX);
    assert_eq!(writer.prot(&Class::InitArray), Prot::R);
    assert_eq!(writer.prot(&Class::Got), Prot::R);
    assert_eq!(writer.prot(&Class::ThreadVars), Prot::RW);
}

#[test]
fn link_sections_and_specials() {
    let writer = super::writer(&executable(Arch::Aarch64)).unwrap();
    let linkme = Class::Named { name: "__DATA,__linkme_X".into(), write: true, exec: false };
    assert_eq!(
        writer.section("__DATA,__linkme_X,regular,no_dead_strip", false, false, false).unwrap(),
        (linkme.clone(), u32::MAX)
    );
    let section = |end| Some(Special::Section { name: "__DATA,__linkme_X".into(), end });
    assert_eq!(writer.special("\x01section$start$__DATA$__linkme_X"), section(false));
    assert_eq!(writer.special("\x01section$end$__DATA$__linkme_X"), section(true));
    assert_eq!(writer.section("__DATA,__mod_init_func,mod_init_funcs", false, false, false).unwrap().0, Class::InitArray);
    assert_eq!(writer.section("__TEXT,__text,regular,pure_instructions", false, true, false).unwrap().0, Class::Text);
    assert_eq!(writer.section("__DATA,__data", true, false, false).unwrap().0, Class::Data);
    assert_eq!(writer.section("__DATA,__thread_data", true, false, true).unwrap().0, Class::Tdata);
    assert_eq!(writer.section("__DATA,__thread_bss,thread_local_zerofill", true, false, true).unwrap().0, Class::Tbss);
    for bad in ["nocomma", "__FOO,__bar", "__DATA,__x,bogus", "__DATA,__x,regular,bogus", "__DATA,__seventeen_bytes_", "__DATA,__nl,non_lazy_symbol_pointers"] {
        assert!(writer.section(bad, true, false, false).is_err(), "{bad}");
    }
    assert_eq!(writer.special("_mh_execute_header"), Some(Special::ImageBase));
    assert_eq!(writer.special("_mh_dylib_header"), None);
    assert_eq!(writer.special("__dso_handle"), Some(Special::ImageBase));
    assert_eq!(writer.special("__blaze_live"), Some(Special::LiveDescriptor));
    assert_eq!(writer.special("\x01section$start$__DATA$__mod_init_func"), Some(Special::Start(Class::InitArray)));
    assert_eq!(writer.special("\x01segment$start$__TEXT"), Some(Special::ImageBase));
    let regions = hello(executable(Arch::Aarch64)).regions;
    let segment = |name: &str| match writer.special(name) {
        Some(Special::Placed(at)) => at(&regions),
        other => panic!("{name}: {other:?}"),
    };
    assert_eq!(segment("\x01segment$end$__TEXT"), GOT);
    assert_eq!(segment("\x01segment$start$__DATA_CONST"), GOT);
    assert_eq!(segment("\x01segment$end$__DATA_CONST"), DATA);
    assert_eq!(segment("\x01segment$start$__DATA"), DATA);
    assert_eq!(segment("\x01segment$end$__DATA"), BASE + 0x10000);
    let no_data_const: Vec<Region> =
        regions.iter().filter(|it| it.prot != Prot::R).cloned().collect();
    let at = |name| match writer.special(name) {
        Some(Special::Placed(at)) => at(&no_data_const),
        other => panic!("{other:?}"),
    };
    assert_eq!((at("segment$start$__DATA_CONST"), at("segment$end$__DATA_CONST")), (GOT, GOT));
    assert_eq!(writer.special("\x01segment$start$__LINKEDIT"), None);
    assert_eq!(writer.special("malloc"), None);
    assert_eq!(writer.special("\x01section$start$__NOPE$__x"), None);

    assert_eq!(super::classify("__LD", "__compact_unwind", 0x0200_0000).unwrap(), SectionKind::Unwind(super::compact_unwind()));
    assert_eq!(super::classify("__TEXT", "__eh_frame", 0x6800_000b).unwrap(), SectionKind::EhFrame);
    assert_eq!(super::classify("__DWARF", "__debug_info", 0x0200_0000).unwrap(), SectionKind::Dropped);
    assert_eq!(super::classify("__TEXT", "__cstring", macho::S_CSTRING_LITERALS.to_flags().0).unwrap(), SectionKind::Alloc(Class::Rodata, u32::MAX));
    assert_eq!(super::classify("__DATA", "__common", macho::S_ZEROFILL.to_flags().0).unwrap(), SectionKind::Alloc(Class::Bss, u32::MAX));
    assert_eq!(super::classify("__DATA", "__thread_vars", macho::S_THREAD_LOCAL_VARIABLES.to_flags().0).unwrap(), SectionKind::Alloc(Class::ThreadVars, u32::MAX));
    assert_eq!(super::classify("__DATA", "__const", macho::S_REGULAR.to_flags().0).unwrap(), SectionKind::Alloc(Class::RelRo, u32::MAX));
    assert_eq!(super::classify("__DATA_CONST", "__const", macho::S_REGULAR.to_flags().0).unwrap(), SectionKind::Alloc(Class::RelRo, u32::MAX));
}

/// What a test image is made of, owned, so a `Placed` can borrow it.
struct Parts {
    output: Output,
    plan: Plan,
    regions: Vec<Region>,
    contents: Vec<Vec<u8>>,
    libraries: Vec<Library>,
    imports: Vec<Import>,
    got: Vec<GotEntry>,
    rebase: Vec<u64>,
    binds: Vec<Bind>,
    exports: Vec<Export>,
    functions: Vec<Function>,
    entry: Option<u64>,
    tls: Option<Tls>,
    debug: Vec<(&'static str, Vec<u8>)>,
}

impl Parts {
    fn slices(&self) -> Vec<&[u8]> {
        self.contents.iter().map(Vec::as_slice).collect()
    }

    fn placed<'a>(&'a self, contents: &'a [&'a [u8]]) -> Placed<'a> {
        Placed {
            output: &self.output,
            name: "hello",
            plan: &self.plan,
            regions: &self.regions,
            contents,
            paged: &[],
            libraries: &self.libraries,
            imports: &self.imports,
            got: &self.got,
            rebase: &self.rebase,
            binds: &self.binds,
            exports: &self.exports,
            functions: &self.functions,
            entry: self.entry,
            init: None,
            fini: None,
            tls: self.tls,
            id: [7; 16],
            debug: &self.debug,
            symbol: &no_symbol,
            synthesis: 0,
            delta: None,
            encoded: &[],
        }
    }
}

fn no_symbol(_: &str) -> Option<u64> {
    None
}

const BASE: u64 = 0x1_0000_0000;
const TEXT: u64 = BASE + 0x4000;
const GOT: u64 = BASE + 0x8000;
const INIT: u64 = GOT + 16;
const DATA: u64 = BASE + 0xc000;
const VARS: u64 = DATA + 16;

fn import(name: &str, library: Option<u32>, weak: bool) -> Import {
    Import { name: name.to_owned(), library, version: None, kind: SymbolKind::Function, weak, wasm: None }
}

/// An arm64 executable: code, a GOT of an import and a local, an initializer, data with a
/// pointer slid and one bound, a thread-local variable's descriptor, thread-locals and zeroed
/// data.
fn hello(output: Output) -> Parts {
    let writer = super::writer(&output).unwrap();
    let plan = writer.plan();
    let unwind = TEXT + 0x100;
    let regions = vec![
        region(Class::Text, TEXT, 8, 0x100, Prot::RX),
        region(Class::Synthetic(super::UNWIND_INFO), unwind, 12, 64, Prot::RX),
        region(Class::Got, GOT, 16, 16, Prot::R),
        region(Class::InitArray, INIT, 8, 8, Prot::R),
        region(Class::Data, DATA, 16, 16, Prot::RW),
        region(Class::ThreadVars, VARS, 24, 24, Prot::RW),
        region(Class::Tdata, VARS + 24, 8, 8, Prot::RW),
        region(Class::Tbss, VARS + 32, 8, 8, Prot::RW),
        region(Class::Bss, VARS + 40, 32, 32, Prot::RW),
    ];
    let contents = vec![
        vec![0x1f, 0x20, 0x03, 0xd5, 0xc0, 0x03, 0x5f, 0xd6],
        vec![1; 12],
        [0u64, TEXT].iter().flat_map(|it| it.to_le_bytes()).collect(),
        TEXT.to_le_bytes().to_vec(),
        [TEXT, 0].iter().flat_map(|it| it.to_le_bytes()).collect(),
        vec![0; 24],
        vec![9; 8],
        Vec::new(),
        Vec::new(),
    ];
    Parts {
        output,
        plan,
        regions,
        contents,
        libraries: vec![
            Library { name: "/usr/lib/libSystem.B.dylib".to_owned(), as_needed: false },
            Library { name: "/usr/lib/libunused.dylib".to_owned(), as_needed: true },
        ],
        imports: vec![
            import("free", Some(0), false),
            import("maybe", None, true),
            import("_tlv_bootstrap", Some(0), false),
        ],
        got: vec![
            GotEntry { addr: GOT, holds: Holds::Import(0) },
            GotEntry { addr: GOT + 8, holds: Holds::Local(TEXT) },
        ],
        rebase: vec![INIT, GOT + 8, DATA],
        binds: vec![
            Bind { addr: DATA + 8, import: 1, addend: 0 },
            Bind { addr: VARS, import: 2, addend: 0 },
        ],
        exports: vec![Export {
            name: "main".to_owned(),
            addr: TEXT,
            size: 8,
            kind: SymbolKind::Function,
        }],
        functions: vec![
            Function { addr: TEXT, size: 4, fde: None, windows: None, name: Some("main".into()) },
            Function {
                addr: TEXT + 4,
                size: 4,
                fde: None,
                windows: None,
                name: Some("_ZN5hello6helper17h0123456789abcdefE".into()),
            },
        ],
        entry: Some(TEXT),
        tls: Some(Tls { addr: VARS + 24, init: 8, size: 16, align: 8 }),
        debug: vec![(".debug_line", vec![1, 2, 3, 4]), (".debug_str_offsets", vec![5; 8])],
    }
}

/// What the load commands of a written image say.
#[derive(Default)]
struct Read {
    flags: u32,
    filetype: u32,
    segments: Vec<(String, u64, u64, u64, u64)>,
    sections: Vec<(String, String, u64, u64, u32)>,
    dylibs: Vec<(u32, String)>,
    id: Option<String>,
    dylinker: Option<String>,
    uuid: Option<[u8; 16]>,
    build: Option<(u32, u32, u32)>,
    entry: Option<u64>,
    info: Option<[(u32, u32); 5]>,
    symtab: Option<(u32, u32, u32, u32)>,
    dysymtab: Option<[u32; 6]>,
    indirect_at: Option<u32>,
    starts: Option<(u32, u32)>,
    in_code: Option<(u32, u32)>,
    signature: Option<(u32, u32)>,
    chained: Option<(u32, u32)>,
    exports_trie: Option<(u32, u32)>,
}

fn name16(bytes: &[u8; 16]) -> String {
    let end = bytes.iter().position(|it| *it == 0).unwrap_or(16);
    String::from_utf8(bytes[..end].to_vec()).unwrap()
}

fn read(file: &[u8]) -> Read {
    use object::read::macho::LoadCommandVariant as V;
    let header = macho::MachHeader64::<LE>::parse(file, 0).expect("a Mach-O header");
    let mut read = Read { flags: header.flags(LE).0, filetype: header.filetype(LE).0, ..Read::default() };
    let mut commands = header.load_commands(LE, file, 0).unwrap();
    while let Some(command) = commands.next().unwrap() {
        match command.variant().unwrap() {
            V::Segment64(segment, data) => {
                read.segments.push((
                    name16(segment.segname()),
                    segment.vmaddr(LE),
                    segment.vmsize(LE),
                    segment.fileoff(LE),
                    segment.filesize(LE),
                ));
                for section in segment.sections(LE, data).unwrap() {
                    read.sections.push((
                        name16(section.segname()),
                        name16(section.sectname()),
                        section.addr(LE),
                        section.size(LE),
                        section.flags(LE).0,
                    ));
                }
            }
            V::Dylib(dylib) => {
                let name = command.string(LE, dylib.dylib.name).unwrap();
                read.dylibs.push((command.cmd().0, String::from_utf8(name.to_vec()).unwrap()));
            }
            V::IdDylib(dylib) => {
                let name = command.string(LE, dylib.dylib.name).unwrap();
                read.id = Some(String::from_utf8(name.to_vec()).unwrap());
            }
            V::LoadDylinker(dylinker) => {
                let name = command.string(LE, dylinker.name).unwrap();
                read.dylinker = Some(String::from_utf8(name.to_vec()).unwrap());
            }
            V::Uuid(uuid) => read.uuid = Some(uuid.uuid),
            V::BuildVersion(build, _) => {
                read.build = Some((build.platform.get(LE).0, build.minos.get(LE).0, build.sdk.get(LE).0))
            }
            V::EntryPoint(entry) => read.entry = Some(entry.entryoff.get(LE)),
            V::DyldInfo(info) => {
                read.info = Some([
                    (info.rebase_off.get(LE), info.rebase_size.get(LE)),
                    (info.bind_off.get(LE), info.bind_size.get(LE)),
                    (info.weak_bind_off.get(LE), info.weak_bind_size.get(LE)),
                    (info.lazy_bind_off.get(LE), info.lazy_bind_size.get(LE)),
                    (info.export_off.get(LE), info.export_size.get(LE)),
                ])
            }
            V::Symtab(symtab) => {
                read.symtab = Some((
                    symtab.symoff.get(LE),
                    symtab.nsyms.get(LE),
                    symtab.stroff.get(LE),
                    symtab.strsize.get(LE),
                ))
            }
            V::Dysymtab(it) => {
                read.dysymtab = Some([
                    it.nlocalsym.get(LE),
                    it.iextdefsym.get(LE),
                    it.nextdefsym.get(LE),
                    it.iundefsym.get(LE),
                    it.nundefsym.get(LE),
                    it.nindirectsyms.get(LE),
                ]);
                read.indirect_at = Some(it.indirectsymoff.get(LE));
            }
            V::LinkeditData(data) => {
                let it = Some((data.dataoff.get(LE), data.datasize.get(LE)));
                match command.cmd() {
                    macho::LC_FUNCTION_STARTS => read.starts = it,
                    macho::LC_DATA_IN_CODE => read.in_code = it,
                    macho::LC_CODE_SIGNATURE => read.signature = it,
                    macho::LC_DYLD_CHAINED_FIXUPS => read.chained = it,
                    macho::LC_DYLD_EXPORTS_TRIE => read.exports_trie = it,
                    other => panic!("linkedit data command {other:#x}"),
                }
            }
            V::SourceVersion(_) => {}
            _ => panic!("a load command {:#x}", command.cmd()),
        }
    }
    read
}

fn range(file: &[u8], (at, len): (u32, u32)) -> &[u8] {
    &file[at as usize..(at + len) as usize]
}

/// (name, type, sect, desc, value) of every symbol.
fn symbols(file: &[u8], read: &Read) -> Vec<(String, u8, u8, u16, u64)> {
    let (at, count, strings, _) = read.symtab.unwrap();
    (0..count as usize)
        .map(|nth| {
            let entry = &file[at as usize + nth * 16..][..16];
            let strx = u32::from_le_bytes(entry[..4].try_into().unwrap()) as usize;
            let mut name_at = strings as usize + strx;
            let name = String::from_utf8(c_string(file, &mut name_at)).unwrap();
            let desc = u16::from_le_bytes(entry[6..8].try_into().unwrap());
            let value = u64::from_le_bytes(entry[8..16].try_into().unwrap());
            (name, entry[4], entry[5], desc, value)
        })
        .collect()
}

#[test]
fn executable_reads_back() {
    let parts = hello(executable(Arch::Aarch64));
    let writer = super::writer(&parts.output).unwrap();
    let slices = parts.slices();
    let placed = parts.placed(&slices);
    let file = written(&writer, &placed);
    let read = read(&file);

    assert_eq!(read.filetype, macho::MH_EXECUTE.0);
    for flag in [macho::MH_PIE.0, macho::MH_DYLDLINK.0, macho::MH_TWOLEVEL.0, macho::MH_HAS_TLV_DESCRIPTORS.0] {
        assert_ne!(read.flags & flag, 0, "{flag:#x}");
    }
    let header = macho::MachHeader64::<LE>::parse(&*file, 0).unwrap();
    assert_eq!(header.cputype(LE), macho::CPU_TYPE_ARM64);

    let names: Vec<&str> = read.segments.iter().map(|it| it.0.as_str()).collect();
    assert_eq!(names, ["__PAGEZERO", "__TEXT", "__DATA_CONST", "__DATA", "__DWARF", "__LINKEDIT"]);
    assert_eq!(read.segments[0], ("__PAGEZERO".to_owned(), 0, BASE, 0, 0));
    assert_eq!(read.segments[1].1, BASE);
    assert_eq!(read.segments[1].3, 0);
    assert_eq!(read.segments[2].1, GOT);
    assert_eq!(read.segments[3].1, DATA);
    // `__DATA` has bytes up to its thread-locals and the rest zeroed.
    let data = &read.segments[3];
    assert!(data.4 < data.2 || data.4 == data.2);
    assert_eq!(data.3, DATA - BASE);
    let linkedit = read.segments.last().unwrap();
    assert_eq!(linkedit.3 % 0x4000, 0);
    assert_eq!(linkedit.3 + linkedit.4, file.len() as u64);

    let sections: Vec<(&str, &str, u64, u64)> = (read.sections.iter())
        .map(|it| (it.0.as_str(), it.1.as_str(), it.2, it.3))
        .collect();
    assert_eq!(
        sections[..9],
        [
            ("__TEXT", "__text", TEXT, 8),
            ("__TEXT", "__unwind_info", TEXT + 0x100, 12),
            ("__DATA_CONST", "__got", GOT, 16),
            ("__DATA_CONST", "__mod_init_func", INIT, 8),
            ("__DATA", "__data", DATA, 16),
            ("__DATA", "__thread_vars", VARS, 24),
            ("__DATA", "__thread_data", VARS + 24, 8),
            ("__DATA", "__thread_bss", VARS + 32, 8),
            ("__DATA", "__bss", VARS + 40, 32),
        ]
    );
    let flags: Vec<u32> = read.sections.iter().map(|it| it.4 & macho::SECTION_TYPE).collect();
    assert_eq!(
        flags[..9],
        [
            macho::S_REGULAR.to_flags().0,
            macho::S_REGULAR.to_flags().0,
            macho::S_NON_LAZY_SYMBOL_POINTERS.to_flags().0,
            macho::S_MOD_INIT_FUNC_POINTERS.to_flags().0,
            macho::S_REGULAR.to_flags().0,
            macho::S_THREAD_LOCAL_VARIABLES.to_flags().0,
            macho::S_THREAD_LOCAL_REGULAR.to_flags().0,
            macho::S_THREAD_LOCAL_ZEROFILL.to_flags().0,
            macho::S_ZEROFILL.to_flags().0,
        ]
    );
    assert_eq!(&sections[9..], [("__DWARF", "__debug_line", linkedit.1 - 0x4000, 4), (
        "__DWARF",
        "__debug_str_offs",
        linkedit.1 - 0x4000 + 4,
        8
    )]);
    // Region bytes are where the sections say.
    assert_eq!(&file[0x4000..0x4008], &parts.contents[0][..]);
    assert_eq!(&file[(DATA - BASE) as usize..][..8], &TEXT.to_le_bytes());

    assert_eq!(read.dylinker.as_deref(), Some("/usr/lib/dyld"));
    assert_eq!(read.dylibs, [(macho::LC_LOAD_DYLIB.0, "/usr/lib/libSystem.B.dylib".to_owned())]);
    assert_eq!(read.uuid, Some([7; 16]));
    assert_eq!(read.build, Some((1, OPCODES, 0x000f_0000)));
    assert_eq!((read.chained, read.exports_trie), (None, None), "macOS 11 has no chained fixups");
    assert_eq!(read.entry, Some(0x4000));

    let info = read.info.unwrap();
    let segments: Vec<u64> = read.segments.iter().map(|it| it.1).collect();
    let mut rebases = decode_rebases(range(&file, info[0]), &segments);
    rebases.sort();
    assert_eq!(rebases, [GOT + 8, INIT, DATA]);
    let binds = decode_binds(range(&file, info[1]), &segments);
    assert_eq!(binds, [
        (GOT, 1, b"_free".to_vec(), 0, 0),
        (DATA + 8, -2, b"_maybe".to_vec(), macho::BIND_SYMBOL_FLAGS_WEAK_IMPORT.0, 0),
        (VARS, 1, b"__tlv_bootstrap".to_vec(), 0, 0),
    ]);
    assert_eq!((info[2].1, info[3].1), (0, 0));
    let trie = range(&file, info[4]);
    assert_eq!(trie_lookup(trie, b"_main"), Some(0x4000));
    assert_eq!(trie_lookup(trie, b"__mh_execute_header"), Some(0));
    assert_eq!(decode_trie(trie).len(), 2);

    let symbols = symbols(&file, &read);
    assert_eq!(read.dysymtab.unwrap(), [1, 1, 2, 3, 3, 2]);
    assert_eq!(symbols[0].0, "__ZN5hello6helper17h0123456789abcdefE");
    assert_eq!((symbols[0].1, symbols[0].2, symbols[0].4), (macho::N_SECT.0, 1, TEXT + 4));
    assert_eq!(symbols[1].0, "__mh_execute_header");
    assert_eq!(symbols[1].3, macho::REFERENCED_DYNAMICALLY.0);
    assert_eq!((symbols[2].0.as_str(), symbols[2].1), ("_main", macho::N_SECT.0 | macho::N_EXT.0));
    assert_eq!((symbols[3].0.as_str(), symbols[3].3 >> 8), ("__tlv_bootstrap", 1));
    assert_eq!((symbols[4].0.as_str(), symbols[4].3 >> 8), ("_free", 1));
    assert_eq!(symbols[5].0, "_maybe");
    assert_eq!(symbols[5].3 >> 8, u16::from(macho::DYNAMIC_LOOKUP_ORDINAL.0));
    assert_ne!(symbols[5].3 & macho::N_WEAK_REF.0, 0);
    let indirect = range(&file, (read.indirect_at.unwrap(), 8));
    assert_eq!(indirect, [4, 0, 0, 0, 0, 0, 0, 0x80].as_slice(), "_free, then a local");

    let starts = range(&file, read.starts.unwrap());
    let mut at = 0;
    assert_eq!((uleb(starts, &mut at), uleb(starts, &mut at), uleb(starts, &mut at)), (0x4000, 4, 0));
    assert_eq!(read.in_code.unwrap().1, 0);

    let (signed, len) = read.signature.unwrap();
    assert_eq!(signed % 16, 0);
    assert_eq!((signed + len) as usize, file.len());
    check_signature(range(&file, (signed, len)), &file[..signed as usize], "hello");

    // `object` reads it as the executable it is.
    use object::{Object as _, ObjectSection as _};
    let parsed = object::read::macho::MachOFile64::<LE>::parse(&*file).unwrap();
    assert_eq!(parsed.entry(), TEXT - BASE);
    assert_eq!(parsed.mach_uuid().unwrap(), Some([7; 16]));
    let text = parsed.section_by_name("__text").unwrap();
    assert_eq!(text.data().unwrap(), &parts.contents[0][..]);
}

#[test]
fn a_dylib_reads_back() {
    let shared = OutputKind::Shared { name: "@rpath/libhello.dylib".to_owned(), exports: Vec::new() };
    let mut parts = hello(output(Arch::X86_64, shared));
    let delta = BASE;
    for region in &mut parts.regions {
        region.addr -= delta;
    }
    for cell in &mut parts.got {
        cell.addr -= delta;
        if let Holds::Local(addr) = &mut cell.holds {
            *addr -= delta;
        }
    }
    for addr in &mut parts.rebase {
        *addr -= delta;
    }
    for bind in &mut parts.binds {
        bind.addr -= delta;
    }
    for function in &mut parts.functions {
        function.addr -= delta;
    }
    parts.exports[0].addr -= delta;
    parts.entry = None;
    parts.tls = parts.tls.map(|it| Tls { addr: it.addr - delta, ..it });
    parts.debug.clear();
    let writer = super::writer(&parts.output).unwrap();
    let slices = parts.slices();
    let placed = parts.placed(&slices);
    let file = written(&writer, &placed);
    let read = read(&file);
    assert_eq!(read.filetype, macho::MH_DYLIB.0);
    assert_eq!(read.flags & macho::MH_PIE.0, 0);
    assert_eq!(read.id.as_deref(), Some("@rpath/libhello.dylib"));
    assert_eq!(read.dylinker, None);
    assert_eq!(read.entry, None);
    assert_eq!(read.segments[0].0, "__TEXT");
    assert_eq!(read.segments[0].1, 0);
    let header = macho::MachHeader64::<LE>::parse(&*file, 0).unwrap();
    assert_eq!(header.cputype(LE), macho::CPU_TYPE_X86_64);
    let trie = range(&file, read.info.unwrap()[4]);
    assert_eq!(decode_trie(trie), [(b"_main".to_vec(), 0, 0x4000)]);
    let (signed, len) = read.signature.unwrap();
    check_signature(range(&file, (signed, len)), &file[..signed as usize], "hello");
}

#[test]
fn weak_dylib_when_every_import_is_weak() {
    let mut parts = hello(executable(Arch::Aarch64));
    parts.imports[0].weak = true;
    let writer = super::writer(&parts.output).unwrap();
    let file = written(&writer, &parts.placed(&parts.slices()));
    assert_eq!(read(&file).dylibs, [(macho::LC_LOAD_DYLIB.0, "/usr/lib/libSystem.B.dylib".to_owned())]);
    parts.imports[2].weak = true;
    let file = written(&writer, &parts.placed(&parts.slices()));
    assert_eq!(read(&file).dylibs, [(macho::LC_LOAD_WEAK_DYLIB.0, "/usr/lib/libSystem.B.dylib".to_owned())]);
}

#[test]
fn errors_name_what() {
    let writer = super::writer(&executable(Arch::Aarch64)).unwrap();

    let mut parts = hello(executable(Arch::Aarch64));
    parts.rebase.push(TEXT);
    let what = writer.file(&parts.placed(&parts.slices())).err().unwrap().what;
    assert!(what.contains("__TEXT"), "{what}");

    let mut parts = hello(executable(Arch::Aarch64));
    parts.got[1].holds = Holds::TpOff(8);
    assert!(writer.file(&parts.placed(&parts.slices())).is_err());

    let mut parts = hello(executable(Arch::Aarch64));
    parts.entry = None;
    assert!(writer.file(&parts.placed(&parts.slices())).is_err());

    let mut parts = hello(executable(Arch::Aarch64));
    parts.contents[7] = vec![1];
    assert!(writer.file(&parts.placed(&parts.slices())).is_err(), "bytes in a zeroed region");

    let mut parts = hello(executable(Arch::Aarch64));
    parts.libraries = (0..300)
        .map(|it| Library { name: format!("/usr/lib/lib{it}.dylib"), as_needed: false })
        .collect();
    assert!(writer.file(&parts.placed(&parts.slices())).is_err(), "too many libraries");

    assert!(writer.apply(1, &mut [0; 4], 0, 0, 0).is_err());
}

#[test]
fn reads_universal_slices() {
    // A universal file of two slices: the arm64 one is taken.
    let mut file = Vec::new();
    for field in [macho::FAT_MAGIC, 2] {
        file.extend_from_slice(&field.to_be_bytes());
    }
    for (cpu, offset) in [(macho::CPU_TYPE_X86_64.0, 64u32), (macho::CPU_TYPE_ARM64.0, 72)] {
        for field in [cpu, 0, offset, 8, 3] {
            file.extend_from_slice(&field.to_be_bytes());
        }
    }
    file.resize(64, 0);
    file.extend_from_slice(b"x86slice");
    file.extend_from_slice(b"armslice");
    let machine = super::MachO {
        output: executable(Arch::Aarch64),
        cpu: macho::CPU_TYPE_ARM64.0,
        subtype: 0,
        page: 0x4000,
        platform: Platform { platform: 1, minimum: 0, sdk: 0 },
        cache: None,
    };
    assert_eq!(machine.thin("fat", &file).unwrap(), (72, &b"armslice"[..]));
    let ppc = super::MachO { cpu: 18, ..machine };
    assert!(ppc.thin("fat", &file).is_err());
    assert_eq!(ppc.thin("thin", b"\xcf\xfa\xed\xfe....").unwrap().0, 0);
}

/// An arm64 object with code, data and C strings in atoms, a local and an undefined symbol,
/// and pointers to both.
fn an_object() -> Vec<u8> {
    use object::write::{Object as Builder, Relocation, StandardSection, Symbol as Def, SymbolSection};
    use object::{
        Architecture, BinaryFormat, Endianness, RelocationFlags, SymbolFlags, SymbolKind as Type,
        SymbolScope,
    };
    let mut b = Builder::new(BinaryFormat::MachO, Architecture::Aarch64, Endianness::Little);
    b.set_subsections_via_symbols();
    let text = b.section_id(StandardSection::Text);
    b.append_section_data(text, &[0x1f, 0x20, 0x03, 0xd5].repeat(8), 4);
    let data = b.section_id(StandardSection::Data);
    b.append_section_data(data, &[0; 16], 8);
    let strings = b.section_id(StandardSection::ReadOnlyString);
    b.append_section_data(strings, b"hello\0world\0", 1);
    let mut symbol = |name: &str, value: u64, size: u64, kind, scope, section| {
        b.add_symbol(Def {
            name: name.into(),
            value,
            size,
            kind,
            scope,
            weak: false,
            section,
            flags: SymbolFlags::None,
        })
    };
    let f = symbol("_f", 0, 16, Type::Text, SymbolScope::Linkage, SymbolSection::Section(text));
    symbol("_g", 16, 16, Type::Text, SymbolScope::Compilation, SymbolSection::Section(text));
    symbol("_d", 0, 16, Type::Data, SymbolScope::Linkage, SymbolSection::Section(data));
    let ext = symbol("_ext", 0, 0, Type::Unknown, SymbolScope::Dynamic, SymbolSection::Undefined);
    for (at, target) in [(0, f), (8, ext)] {
        let flags =
            RelocationFlags::MachO { r_type: macho::ARM64_RELOC_UNSIGNED, r_pcrel: false, r_length: 3 };
        b.add_relocation(data, Relocation { offset: at, symbol: target, addend: 0, flags }).unwrap();
    }
    b.write().unwrap()
}

fn native_object(read: Result<crate::model::NativeFile, String>) -> crate::model::Object {
    match read {
        Ok(crate::model::NativeFile::Object(object)) => object,
        Ok(_) => panic!("read as something other than an object"),
        Err(error) => panic!("{error}"),
    }
}

/// `a`, read from `a_input`, and `b`, from `b_input`, say the same through every accessor.
fn same_object(
    a: &crate::model::Object,
    a_input: &[u8],
    b: &crate::model::Object,
    b_input: &[u8],
) {
    assert_eq!(a.sections.len(), b.sections.len());
    for (x, y) in a.sections.iter().zip(&b.sections) {
        assert_eq!(a.name(x.name), b.name(y.name));
        assert_eq!((&x.what, x.align, x.retain), (&y.what, y.align, y.retain));
        assert_eq!(x.data.len(), y.data.len());
        assert_eq!(x.data.bytes(a_input).unwrap(), y.data.bytes(b_input).unwrap(), "{}", a.name(x.name));
        assert_eq!(a.section_fixups(x), b.section_fixups(y));
    }
    assert_eq!(a.symbols, b.symbols);
    assert_eq!(a.names, b.names);
    assert_eq!(a.libraries, b.libraries);
}

#[test]
fn objects_are_read_in_place_wherever_they_start() {
    use crate::model::Data;
    let bytes = an_object();
    let writer = super::writer(&executable(Arch::Aarch64)).unwrap();
    let thin = native_object(writer.read("x.o", &bytes, &crate::NativeKind::Object));
    let mut held = 0;
    for section in &thin.sections {
        match &section.data {
            Data::Input(range) => {
                assert!(range.start <= range.end && range.end <= bytes.len() as u64);
                held += range.end - range.start;
            }
            Data::Zero(_) => {}
            Data::Bytes(_) => panic!("{}: copied, not a range of the input", thin.name(section.name)),
            Data::Made(_) => panic!("{}: made, not a range of the input", thin.name(section.name)),
        }
    }
    assert_eq!(held, 32 + 16 + 12, "every byte of the code, the data and the strings");
    // An archive's member starts at any byte: every start of a word reads the same.
    let mut buffer = vec![0u8; bytes.len() + 8];
    for start in 0..8 {
        buffer[start..start + bytes.len()].copy_from_slice(&bytes);
        let input = &buffer[start..start + bytes.len()];
        let read = native_object(writer.read("x.o", input, &crate::NativeKind::Object));
        same_object(&thin, &bytes, &read, input);
        for (x, y) in thin.sections.iter().zip(&read.sections) {
            assert_eq!(x.data, y.data, "ranges of the slice read, wherever it is");
        }
    }
}

#[test]
fn universal_objects_name_bytes_of_the_whole_file() {
    use crate::model::Data;
    let bytes = an_object();
    let at = 0x4000u32;
    let mut fat = Vec::new();
    for field in [macho::FAT_MAGIC, 2] {
        fat.extend_from_slice(&field.to_be_bytes());
    }
    for (cpu, offset, size, align) in
        [(macho::CPU_TYPE_X86_64.0, 0x1000, 8, 12), (macho::CPU_TYPE_ARM64.0, at, bytes.len() as u32, 14)]
    {
        for field in [cpu, 0, offset, size, align] {
            fat.extend_from_slice(&field.to_be_bytes());
        }
    }
    fat.resize(0x1000, 0);
    fat.extend_from_slice(b"x86slice");
    fat.resize(at as usize, 0);
    fat.extend_from_slice(&bytes);

    let writer = super::writer(&executable(Arch::Aarch64)).unwrap();
    let thin = native_object(writer.read("x.o", &bytes, &crate::NativeKind::Object));
    let read = native_object(writer.read("fat.o", &fat, &crate::NativeKind::Object));
    same_object(&thin, &bytes, &read, &fat);
    for (x, y) in thin.sections.iter().zip(&read.sections) {
        match (&x.data, &y.data) {
            (Data::Input(x), Data::Input(y)) => {
                assert_eq!(*y, x.start + u64::from(at)..x.end + u64::from(at));
            }
            (x, y) => assert_eq!(x, y),
        }
    }
}

/// The trie as it was first built, one name at a time with an edge label copied per edge:
/// the reference the one-pass builder must match byte for byte.
fn trie_by_insertion(exports: &mut [(Vec<u8>, u64, u64)]) -> Vec<u8> {
    #[derive(Default)]
    struct Node {
        edges: Vec<(Vec<u8>, usize)>,
        terminal: Option<(u64, u64)>,
        offset: u64,
    }
    let uleb_len = |value: u64| blaze_format::leb::len_u64(value) as u64;
    let info_len = |node: &Node| node.terminal.map_or(0, |(f, a)| uleb_len(f) + uleb_len(a));
    exports.sort_unstable();
    let mut nodes = vec![Node::default()];
    for (name, flags, addr) in exports.iter() {
        let (mut node, mut name) = (0, &name[..]);
        loop {
            if name.is_empty() {
                nodes[node].terminal = Some((*flags, *addr));
                break;
            }
            let Some(edge) = nodes[node].edges.iter().position(|it| it.0[0] == name[0]) else {
                nodes.push(Node { terminal: Some((*flags, *addr)), ..Node::default() });
                let child = nodes.len() - 1;
                nodes[node].edges.push((name.to_vec(), child));
                break;
            };
            let (label, child) = nodes[node].edges[edge].clone();
            let common = label.iter().zip(name).take_while(|(a, b)| a == b).count();
            if common < label.len() {
                nodes.push(Node { edges: vec![(label[common..].to_vec(), child)], ..Node::default() });
                let middle = nodes.len() - 1;
                nodes[node].edges[edge] = (label[..common].to_vec(), middle);
                node = middle;
            } else {
                node = child;
            }
            name = &name[common..];
        }
    }
    for node in &mut nodes {
        node.edges.sort_unstable();
    }
    let mut order = Vec::new();
    let mut stack = vec![0];
    while let Some(node) = stack.pop() {
        order.push(node);
        stack.extend(nodes[node].edges.iter().rev().map(|it| it.1));
    }
    loop {
        let (mut offset, mut moved) = (0, false);
        for &node in &order {
            moved |= nodes[node].offset != offset;
            nodes[node].offset = offset;
            let info = info_len(&nodes[node]);
            let mut len = uleb_len(info) + info + 1;
            for (label, child) in &nodes[node].edges {
                len += label.len() as u64 + 1 + uleb_len(nodes[*child].offset);
            }
            offset += len;
        }
        if !moved {
            break;
        }
    }
    let mut out = Vec::new();
    for &node in &order {
        let node = &nodes[node];
        linkedit::uleb(&mut out, info_len(node));
        if let Some((flags, addr)) = node.terminal {
            linkedit::uleb(&mut out, flags);
            linkedit::uleb(&mut out, addr);
        }
        out.push(node.edges.len() as u8);
        for (label, child) in &node.edges {
            out.extend_from_slice(label);
            out.push(0);
            linkedit::uleb(&mut out, nodes[*child].offset);
        }
    }
    out.resize(out.len().next_multiple_of(8), 0);
    out
}

#[test]
fn trie_is_the_insertion_trie() {
    let mut rng = blaze_ids::rng::SplitMix64::new(0x9e37_79b9_7f4a_7c15);
    let mut next = move || rng.next_u64();
    for round in 0..200 {
        // Names over a small alphabet, so prefixes are shared, split and ended often.
        let count = 1 + (next() % 300) as usize;
        let mut names = std::collections::BTreeSet::new();
        while names.len() < count {
            let len = 1 + (next() % 12) as usize;
            names.insert((0..len).map(|_| b"_abc$Z"[(next() % 6) as usize]).collect::<Vec<u8>>());
        }
        let mut exports: Vec<(Vec<u8>, u64, u64)> = (names.into_iter())
            .map(|name| (name, next() % 3, next() % (1 << (next() % 40))))
            .collect();
        // Out of order, as the builder must sort them itself.
        exports.reverse();
        let want = trie_by_insertion(&mut exports.clone());
        assert_eq!(encode_trie(&mut exports).unwrap(), want, "round {round}");
        let mut borrowed: Vec<(&[u8], u64, u64)> =
            exports.iter().map(|it| (&it.0[..], it.1, it.2)).collect();
        assert_eq!(encode_trie(&mut borrowed).unwrap(), want, "round {round}, borrowed");
    }
}

#[test]
fn names_sort_as_their_mach_o_spelling() {
    let names = ["main", "\x01main", "\x01_main", "_main", "\x01", "a", "\x01a", "\x01`", "Z", "\x01Z"];
    for a in names {
        for b in names {
            let want = macho_name(a).into_bytes().cmp(&macho_name(b).into_bytes());
            assert_eq!(super::macho_order(a, b), want, "{a:?} {b:?}");
        }
    }
}

#[test]
fn rebases_and_starts_sorted_or_not_are_the_same() {
    let sorted: Vec<u64> = (0..50).map(|it| 0x1_0000_8000 + it * 8 + (it / 7) * 0x40).collect();
    let mut shuffled = sorted.clone();
    shuffled.reverse();
    shuffled.swap(3, 30);
    assert_eq!(encode_rebases(&sorted, &spans()).unwrap(), encode_rebases(&shuffled, &spans()).unwrap());
    assert_eq!(
        encode_starts(0x1_0000_0000, &sorted),
        encode_starts(0x1_0000_0000, &shuffled)
    );
    let mut twice = sorted.clone();
    twice.push(sorted[10]);
    assert_eq!(encode_starts(0x1_0000_0000, &twice), encode_starts(0x1_0000_0000, &sorted));
}

/// What the core says changed since the link `from`: nothing moved, `written` rewritten.
fn delta<'a>(from: u64, same_names: bool, written: &'a [(u64, u64)], functions: &'a [Splice]) -> Delta<'a> {
    Delta { functions, rebase: &[], got: &[], binds: &[], same_names, same_regions: true, written, from }
}

/// A writer that keeps its last link writes the same file as one that doesn't, link after
/// link, as the code and the data change: the signature hashes only the pages the core says
/// it wrote and the headers' and `__LINKEDIT`'s that differ, and the tables are the kept
/// ones where their inputs didn't change. Again at chained fixups' deployment target.
#[test]
fn the_signing_cache_writes_the_same_file() {
    for minimum in [OPCODES, CHAINED] {
        let mut incremental = output_at(Arch::Aarch64, OutputKind::Executable { entry: "main".to_owned() }, minimum);
        incremental.incremental = true;
        let cached = super::writer(&incremental).unwrap();
        let mut parts = hello(output_at(Arch::Aarch64, OutputKind::Executable { entry: "main".to_owned() }, minimum));
        for round in 0..5u8 {
            let (wrote, same_names): (&[(u64, u64)], bool) = match round {
                1 => {
                    parts.contents[0][0] ^= 0x10;
                    (&[(TEXT, 1)], true)
                }
                2 => {
                    parts.contents[4][3] = 0x42;
                    (&[(DATA + 3, 1)], true)
                }
                3 => {
                    parts.exports[0].name = "main2".to_owned();
                    (&[], false)
                }
                // The slid pointer in `__DATA` changes its target: its slot, chained, too.
                4 => {
                    parts.contents[4][..8].copy_from_slice(&(TEXT + 4).to_le_bytes());
                    (&[(DATA, 8)], true)
                }
                _ => (&[], true),
            };
            let fresh = super::writer(&parts.output).unwrap();
            let slices = parts.slices();
            let since = delta(u64::from(round), same_names, wrote, &[]);
            let placed = Placed {
                synthesis: u64::from(round) + 1,
                delta: (round != 0).then_some(&since),
                ..parts.placed(&slices)
            };
            let file = written(&cached, &placed);
            assert_eq!(file, written(&fresh, &parts.placed(&slices)), "round {round} at {minimum:#x}");
            let read = read(&file);
            let (at, len) = read.signature.unwrap();
            check_signature(range(&file, (at, len)), &file[..at as usize], "hello");
        }
    }
}

/// A relink that changes a function in place patches the kept symbol table (its buffer the
/// same) and keeps the rebase opcodes, and the file is a full build's.
#[test]
fn a_relink_patches_the_symbol_table_as_a_full_build_writes_it() {
    let mut incremental = executable(Arch::Aarch64);
    incremental.incremental = true;
    let cached = super::writer(&incremental).unwrap();
    let mut parts = hello(executable(Arch::Aarch64));
    let slices = parts.slices();
    let first = Placed { synthesis: 1, ..parts.placed(&slices) };
    written(&cached, &first);
    let nlist = |writer: &super::MachO| {
        let kept = writer.cache.as_ref().unwrap().lock();
        kept.symbols.as_ref().map(|it| it.nlist.as_ptr() as usize)
    };
    let before = nlist(&cached);
    assert!(before.is_some());

    // The helper moves and is renamed to a name of the same length.
    parts.functions[1].addr = TEXT + 6;
    parts.functions[1].name = Some("_ZN5hello6helper17hfedcba9876543210E".into());
    let splices = [Splice { old: 1..2, new: 1..2 }];
    let since = delta(1, true, &[], &splices);
    let slices = parts.slices();
    let second = Placed { synthesis: 2, delta: Some(&since), ..parts.placed(&slices) };
    let file = written(&cached, &second);
    assert_eq!(nlist(&cached), before, "patched in place, not built again");
    let fresh = super::writer(&parts.output).unwrap();
    assert_eq!(file, written(&fresh, &parts.placed(&slices)));
    let read = read(&file);
    let names: Vec<(String, u64)> = symbols(&file, &read).into_iter().map(|it| (it.0, it.4)).collect();
    assert!(names.contains(&("__ZN5hello6helper17hfedcba9876543210E".to_owned(), TEXT + 6)));

    // A name of another length is built again, and still a full build's bytes.
    parts.functions[1].name = Some("_ZN5hello5other17hfedcba9876543210E".into());
    let since = delta(2, true, &[], &splices);
    let slices = parts.slices();
    let third = Placed { synthesis: 3, delta: Some(&since), ..parts.placed(&slices) };
    let file = written(&cached, &third);
    assert_ne!(nlist(&cached), before);
    assert_eq!(file, written(&super::writer(&parts.output).unwrap(), &parts.placed(&slices)));

    // A delta from a link the writer didn't keep (a rollback) patches nothing.
    let since = delta(1, true, &[], &[]);
    let fourth = Placed { synthesis: 4, delta: Some(&since), ..parts.placed(&slices) };
    assert_eq!(written(&cached, &fourth), written(&super::writer(&parts.output).unwrap(), &parts.placed(&slices)));
}

#[test]
fn an_object_read_allocates_a_constant_number_of_buffers() {
    // Each sized once: the nlist names, the name arena, the sections as read, the symbols'
    // places, the atom bounds, the atom starts, the atoms, their section symbols, the nlist
    // map, the symbols, the fixups, `__eh_frame`'s sections by address, the fixup column. A
    // radix sort of more than 64 keys out of order would add three; these are in order.
    let used = [100, 2000].map(|n| {
        let bytes = super::input::tests::sized(n);
        let (object, used) =
            blaze_alloc_count::measure(|| super::input::read("sized.o", &bytes, Arch::X86_64));
        let object = object.unwrap();
        assert_eq!(object.sections.len(), n + 2);
        assert_eq!(object.fixups.len(), 2 * n);
        used.allocations
    });
    assert_eq!(used, [13, 13]);
}

#[test]
fn an_archive_index_read_is_bounded_by_its_index() {
    let symbols = ["_alpha", "_beta", "_gamma", "_delta"];
    let (mut strings, mut offsets) = (Vec::new(), Vec::new());
    for symbol in symbols {
        offsets.push(strings.len() as u32);
        strings.extend_from_slice(symbol.as_bytes());
        strings.push(0);
    }
    let names = "a.o".len() + symbols.iter().map(|it| core_name(it.as_bytes()).unwrap().len()).sum::<usize>();
    // The members, their header offsets, their ranges, the index, its entries by string
    // offset and the names, and the radix sort's scratch: the entries are out of offset order.
    let used = [128, 8192].map(|entries| {
        let entries: Vec<(u32, usize)> = (0..entries).map(|it| (offsets[3 - it % 4], 0)).collect();
        let bytes = super::archive::tests::bsd_raw(&[("a.o", &b"aaaa"[..])], strings.clone(), &entries);
        let (archive, used) = blaze_alloc_count::measure(|| super::archive::read("libsym.a", &bytes));
        let archive = archive.unwrap();
        assert_eq!(archive.index.len(), entries.len());
        assert_eq!(archive.names.bytes(), names, "one name a string, however many entries name it");
        used.allocations
    });
    assert_eq!(used, [7, 7]);
}

#[test]
fn signing_allocates_nothing_per_page() {
    // One worker, measured on it: the pages are hashed on the thread that signs.
    let pool = blaze_graph::pool::Pool::new(&blaze_graph::topology::Topology::flat(1), false).unwrap();
    for pages in [3, 300] {
        let file: Vec<u8> = (0..pages * sign::PAGE + 100).map(|it| (it * 31 % 251) as u8).collect();
        // Two parts, so the page across them is gathered.
        let (a, b) = file.split_at(pages * sign::PAGE / 2 + 7);
        let view = sign::View::new(file.len() as u64, vec![(0, a), (a.len() as u64, b)]).unwrap();
        let mut cache = sign::Cache::default();
        let len = sign::len("x", file.len() as u64);
        // The pages kept: the headers' and the last two (as `__LINKEDIT`'s).
        let kept = [(0, 0x4000), (file.len() as u64 - 2 * sign::PAGE as u64, 2 * sign::PAGE as u64)];
        pool.install(|| {
            let cold = sign::Cached { cache: &mut cache, synthesis: 1, since: None, kept: &kept };
            sign::signature(&view, "x", 0, 0x4000, true, Some(cold), &mut Vec::new());
            let mut out = Vec::new();
            let ((), plain) =
                blaze_alloc_count::measure(|| sign::signature(&view, "x", 0, 0x4000, true, None, &mut out));
            assert_eq!((plain.allocations, plain.bytes), (1, len), "{pages} pages: the output alone");
            check_signature(&out, &file, "x");
            let want = std::mem::take(&mut out);
            out.reserve(want.len());
            let ((), cached) = blaze_alloc_count::measure(|| {
                let since = Some(sign::Since { from: 1, written: &[] });
                let warm = sign::Cached { cache: &mut cache, synthesis: 2, since, kept: &kept };
                sign::signature(&view, "x", 0, 0x4000, true, Some(warm), &mut out);
            });
            assert_eq!(cached.allocations, 0, "{pages} pages, cached, into a buffer of room");
            assert_eq!(out, want);
        });
    }
}

#[test]
fn linkedit_encodes_into_one_buffer() {
    const IMPORTS: [&[u8]; 4] = [b"_malloc", b"_free", b"_open", b"_close"];
    let spans = spans();
    // One worker, measured on it: the rebases are checked on the pool's threads.
    let pool = blaze_graph::pool::Pool::new(&blaze_graph::topology::Topology::flat(1), false).unwrap();
    for n in [100u64, 2000] {
        // In order, as `Placed` lists them.
        let rebases: Vec<u64> = (0..n).map(|it| 0x1_0000_8000 + 8 * it).collect();
        let functions: Vec<u64> = (0..n).map(|it| 0x1_0000_4000 + 16 * it).collect();
        let mut binds: Vec<linkedit::Bind<'_>> = (0..n)
            .map(|it| linkedit::Bind {
                addr: 0x1_0000_8000 + 8 * it,
                ordinal: 1 + (it % 3) as i64,
                name: IMPORTS[(it % 4) as usize],
                weak: false,
                addend: 0,
            })
            .collect();
        let names: Vec<Vec<u8>> = (0..n).map(|it| format!("_export{it}").into_bytes()).collect();
        let mut exports: Vec<(&[u8], u64, u64)> =
            names.iter().zip(0..).map(|(name, it)| (&name[..], 0, 0x4000 + 16 * it)).collect();
        exports.sort_unstable();
        let mut out =
            Vec::with_capacity(linkedit::capacity(rebases.len(), &binds, &exports, functions.len()));
        let (at, room) = (out.as_ptr(), out.capacity());
        let (len, used) = pool.install(|| {
            blaze_alloc_count::measure(|| {
                linkedit::rebases(&rebases, &spans, &mut out).unwrap()
                    + linkedit::binds(&mut binds, &spans, &mut out).unwrap()
                    + linkedit::trie(&mut exports, &mut out).unwrap()
                    + linkedit::function_starts(0x1_0000_0000, &functions, &mut out)
            })
        });
        assert_eq!(
            used.allocations, 6,
            "{n}: the trie's four scratch columns and each stream's cuts"
        );
        assert_eq!((out.as_ptr(), out.capacity()), (at, room), "{n}: the buffer never grew");
        assert_eq!(out.len(), len);
    }
}

/// The locals as `MachO::symbols` wrote them one at a time before they were filled in parallel.
fn locals_one_by_one(
    locals: &[(u64, &str)],
    section_at: &dyn Fn(u64, &str) -> Result<u8, crate::LinkError>,
    strings: &mut Vec<u8>,
    nlist: &mut Vec<u8>,
) -> Result<(), crate::LinkError> {
    for &(addr, name) in locals {
        let sect = section_at(addr, name)?;
        nlist.extend_from_slice(&(strings.len() as u32).to_le_bytes());
        let (prefix, rest) = super::spelled(name);
        strings.extend_from_slice(prefix.as_bytes());
        strings.extend_from_slice(rest.as_bytes());
        strings.push(0);
        nlist.push(macho::N_SECT.0);
        nlist.push(sect);
        nlist.extend_from_slice(&0u16.to_le_bytes());
        nlist.extend_from_slice(&addr.to_le_bytes());
    }
    Ok(())
}

#[test]
fn the_locals_in_parallel_are_the_locals_one_by_one() {
    let mut rng = blaze_ids::rng::SplitMix64::new(0x9e37_79b9_7f4a_7c15);
    let mut next = move || rng.next_u64();
    let names: Vec<String> = (0..10_000)
        .map(|nth| {
            let len = (next() % 120) as usize;
            let body: String = (0..len).map(|_| char::from(b'a' + (next() % 26) as u8)).collect();
            if nth % 5 == 0 { format!("\x01{body}") } else { body }
        })
        .collect();
    let sect = |addr: u64, _: &str| -> Result<u8, crate::LinkError> { Ok((addr % 7) as u8 + 1) };
    let failing = |addr: u64, name: &str| -> Result<u8, crate::LinkError> {
        if addr % 4099 == 4098 {
            return Err(crate::LinkError { function: None, what: format!("{name} at {addr:#x}"), refusal: None });
        }
        Ok(1)
    };
    for count in [0, 1, 4095, 4096, 4097, 10_000] {
        let locals: Vec<(u64, &str)> =
            (names[..count].iter().enumerate()).map(|(nth, it)| (0x1000 + nth as u64, it.as_str())).collect();
        let (mut strings, mut nlist) = (b" \0".to_vec(), vec![0xaa; 32]);
        let (mut want_strings, mut want_nlist) = (strings.clone(), nlist.clone());
        super::locals_into(&locals, &sect, &mut strings, &mut nlist).unwrap();
        locals_one_by_one(&locals, &sect, &mut want_strings, &mut want_nlist).unwrap();
        assert_eq!(strings, want_strings, "{count} locals: the names");
        assert_eq!(nlist, want_nlist, "{count} locals: the entries");

        let ours = super::locals_into(&locals, &failing, &mut Vec::new(), &mut Vec::new());
        let theirs = locals_one_by_one(&locals, &failing, &mut Vec::new(), &mut Vec::new());
        assert_eq!(ours.map_err(|it| it.what), theirs.map_err(|it| it.what), "{count} locals: the first error");
    }
}

fn chained_executable() -> Output {
    output_at(Arch::Aarch64, OutputKind::Executable { entry: "main".to_owned() }, CHAINED)
}

/// What dyld makes of one fixed-up slot: the address it slides to, or the import it binds
/// (name, library ordinal, weak, addend).
#[derive(Clone, PartialEq, Eq, Debug)]
enum Fixup {
    Rebase(u64),
    Bind(Vec<u8>, i64, bool, i64),
}

/// What dyld does with `LC_DYLD_CHAINED_FIXUPS` of `DYLD_CHAINED_PTR_64_OFFSET`, decoded by
/// hand (the workspace's `object` reads no chains): each page's chain from its start, each
/// slot's fixup by address.
fn decode_chained(file: &[u8], read: &Read, base: u64) -> Vec<(u64, Fixup)> {
    let blob = range(file, read.chained.expect("LC_DYLD_CHAINED_FIXUPS"));
    let u16_at = |at: usize| u16::from_le_bytes(blob[at..at + 2].try_into().unwrap());
    let u32_at = |at: usize| u32::from_le_bytes(blob[at..at + 4].try_into().unwrap());
    assert_eq!(u32_at(0), 0, "fixups_version");
    let (starts, imports_at, symbols_at) = (u32_at(4) as usize, u32_at(8) as usize, u32_at(12) as usize);
    let (count, format) = (u32_at(16) as usize, u32_at(20));
    assert_eq!(u32_at(24), 0, "uncompressed names");
    let imports: Vec<(Vec<u8>, i64, bool, i64)> = (0..count)
        .map(|nth| {
            let (word, addend) = match format {
                1 => (u32_at(imports_at + 4 * nth), 0),
                2 => (u32_at(imports_at + 8 * nth), i64::from(u32_at(imports_at + 8 * nth + 4) as i32)),
                other => panic!("import format {other}"),
            };
            let mut at = symbols_at + (word >> 9) as usize;
            (c_string(blob, &mut at), i64::from(word as u8 as i8), word & 0x100 != 0, addend)
        })
        .collect();
    let segments = u32_at(starts) as usize;
    assert_eq!(segments, read.segments.len(), "a start for every segment");
    let mut found = Vec::new();
    for segment in 0..segments {
        let offset = u32_at(starts + 4 + 4 * segment) as usize;
        if offset == 0 {
            continue;
        }
        let at = starts + offset;
        assert_eq!((u16_at(at + 4), u16_at(at + 6)), (0x4000, 6), "16 KiB pages of 64-bit offsets");
        let segment_offset = u64::from_le_bytes(blob[at + 8..at + 16].try_into().unwrap());
        assert_eq!(base + segment_offset, read.segments[segment].1, "the segment's start");
        for page in 0..u64::from(u16_at(at + 20)) {
            let start = u16_at(at + 22 + 2 * page as usize);
            if start == 0xffff {
                continue;
            }
            let mut addr = base + segment_offset + page * 0x4000 + u64::from(start);
            loop {
                let at = (addr - base) as usize;
                let raw = u64::from_le_bytes(file[at..at + 8].try_into().unwrap());
                if raw >> 63 == 1 {
                    let (name, ordinal, weak, addend) = imports[(raw & 0xff_ffff) as usize].clone();
                    let addend = addend + i64::from((raw >> 24) as u8 as i8);
                    found.push((addr, Fixup::Bind(name, ordinal, weak, addend)));
                } else {
                    let high8 = (raw >> 36) & 0xff;
                    found.push((addr, Fixup::Rebase(base + (raw & 0xf_ffff_ffff) + (high8 << 56))));
                }
                let next = (raw >> 51) & 0xfff;
                if next == 0 {
                    break;
                }
                addr += next * 4;
            }
        }
    }
    found
}

/// At macOS 12 an arm64 executable is fixed up by chains: the slots slide and bind what the
/// opcodes of the same image at macOS 11 do, and the trie is the same.
#[test]
fn chained_fixups_slide_and_bind_what_the_opcodes_do() {
    let file_of = |parts: &Parts| {
        let writer = super::writer(&parts.output).unwrap();
        let slices = parts.slices();
        written(&writer, &parts.placed(&slices))
    };
    let (old, new) = (file_of(&hello(executable(Arch::Aarch64))), file_of(&hello(chained_executable())));
    let (old_read, new_read) = (read(&old), read(&new));
    assert_eq!(new_read.info, None, "no LC_DYLD_INFO_ONLY");
    assert_eq!(new_read.build, Some((1, CHAINED, 0x000f_0000)));

    let segments: Vec<u64> = old_read.segments.iter().map(|it| it.1).collect();
    let info = old_read.info.unwrap();
    let held = |addr: u64| u64::from_le_bytes(old[(addr - BASE) as usize..][..8].try_into().unwrap());
    let mut want: Vec<(u64, Fixup)> =
        decode_rebases(range(&old, info[0]), &segments).into_iter().map(|it| (it, Fixup::Rebase(held(it)))).collect();
    want.extend(decode_binds(range(&old, info[1]), &segments).into_iter().map(|(addr, ordinal, name, flags, addend)| {
        (addr, Fixup::Bind(name, ordinal, flags & macho::BIND_SYMBOL_FLAGS_WEAK_IMPORT.0 != 0, addend))
    }));
    want.sort_by_key(|it| it.0);
    assert_eq!(want.len(), 6);
    assert_eq!(decode_chained(&new, &new_read, BASE), want);
    assert_eq!(decode_trie(range(&new, new_read.exports_trie.unwrap())), decode_trie(range(&old, info[4])));
    let (at, len) = new_read.signature.unwrap();
    check_signature(range(&new, (at, len)), &new[..at as usize], "hello");
}

/// dyld's page-in linking maps every page a segment's chained starts count from the file: they
/// count the pages of its file bytes, never its zero-fill ones, which mapped from the file would
/// hold `__LINKEDIT`'s bytes (std's `ENV_LOCK` in `__bss` read as a locked queue).
#[test]
fn chained_starts_count_no_zero_fill_page() {
    let mut parts = hello(chained_executable());
    let bss = parts.regions.iter_mut().find(|it| it.class == Class::Bss).unwrap();
    (bss.size, bss.capacity) = (0xc000, 0xc000);
    let writer = super::writer(&parts.output).unwrap();
    let slices = parts.slices();
    let file = written(&writer, &parts.placed(&slices));
    let segments = segments_of(&file);
    let data = segments.iter().find(|it| it.0 == "__DATA").unwrap();
    assert!(data.2 >= data.4 + 0x8000, "`__DATA` has zero-fill pages: {data:?}");

    let table = range(&file, read(&file).chained.unwrap());
    let u32_at = |at: usize| u32::from_le_bytes(table[at..at + 4].try_into().unwrap());
    let starts = u32_at(4) as usize;
    let mut counted = 0;
    for (nth, segment) in segments.iter().enumerate().take(u32_at(starts) as usize) {
        let offset = u32_at(starts + 4 + 4 * nth) as usize;
        if offset == 0 {
            continue;
        }
        let pages = u16::from_le_bytes(table[starts + offset + 20..][..2].try_into().unwrap());
        assert_eq!(u64::from(pages), segment.4.div_ceil(0x4000), "{}'s pages in the file", segment.0);
        counted += 1;
    }
    assert_eq!(counted, 2, "`__DATA_CONST` and `__DATA` have slots");
    assert_eq!(decode_chained(&file, &read(&file), BASE).len(), 6);
}

/// A bind added after the last slot of a page rewrites two slots: its own, and the one
/// before it, whose `next` now reaches it. The rest are as they were.
#[test]
fn a_bind_added_rewrites_two_slots() {
    let mut parts = hello(chained_executable());
    let writer = super::writer(&parts.output).unwrap();
    let slices = parts.slices();
    let before = writer.encode(&parts.placed(&slices)).unwrap();
    parts.binds.push(Bind { addr: VARS + 16, import: 0, addend: 0 });
    let slices = parts.slices();
    let after = writer.encode(&parts.placed(&slices)).unwrap();
    assert_eq!(after.len(), before.len() + 1);
    let changed: Vec<u64> = after.iter().filter(|it| !before.contains(it)).map(|it| it.0).collect();
    assert_eq!(changed, [VARS, VARS + 16]);
}

/// The debug sections are the core's, laid where `File::debug` says: the file has their
/// bytes, and writing it allocates as many buffers of the same sizes (but the signature's,
/// a hash per page) whatever their size.
#[test]
fn the_debug_sections_are_the_cores_not_copies() {
    let pool = blaze_graph::pool::Pool::new(&blaze_graph::topology::Topology::flat(1), false).unwrap();
    let sizes = [1usize << 20, 3 << 20];
    let used = sizes.map(|size| {
        let mut parts = hello(executable(Arch::Aarch64));
        parts.debug = vec![(".debug_line", vec![3; size]), (".debug_abbrev", Vec::new()), (".debug_str", vec![5; size / 2])];
        let writer = super::writer(&parts.output).unwrap();
        let slices = parts.slices();
        let placed = parts.placed(&slices);
        let (file, used) = pool.install(|| blaze_alloc_count::measure(|| writer.file(&placed)));
        let laid: Vec<usize> = file.unwrap().debug.iter().map(|it| it.1).collect();
        assert_eq!(laid, [0, 2], "the empty section isn't laid");
        let bytes = written(&writer, &placed);
        let read = read(&bytes);
        let dwarf = read.segments.iter().find(|it| it.0 == "__DWARF").unwrap();
        let sections: Vec<_> = read.sections.iter().filter(|it| it.0 == "__DWARF").collect();
        assert_eq!(sections.len(), 2);
        for (section, nth) in sections.into_iter().zip([0, 2]) {
            let at = (section.2 - dwarf.1 + dwarf.3) as usize;
            assert_eq!(&bytes[at..at + section.3 as usize], &parts.debug[nth].1[..]);
        }
        let (at, len) = read.signature.unwrap();
        check_signature(range(&bytes, (at, len)), &bytes[..at as usize], "hello");
        (used.allocations, used.bytes)
    });
    assert_eq!(used[0].0, used[1].0, "as many buffers");
    let pages = (sizes[1] + sizes[1] / 2 - sizes[0] - sizes[0] / 2) / sign::PAGE;
    assert!(used[1].1 - used[0].1 <= (pages * 32 + 16) as u64, "{used:?}: only the signature grows");
}

const ARENA: u64 = BASE + 0x10000;
const ASKED: DevRun = DevRun { arena: 0x10_0000, cells: 0x4000, fixed: 0x8000 };
const CELLS: u64 = ARENA + ASKED.arena;
const FIXED: u64 = CELLS + ASKED.cells;
const PATCH: u64 = TEXT + 0x80;

fn dev_run(minimum: u32) -> Output {
    let executable = OutputKind::Executable { entry: "main".to_owned() };
    Output { dev_run: Some(ASKED), ..output_at(Arch::Aarch64, executable, minimum) }
}

/// `hello` as a dev run links it: a patch region after its code, and its reservations after
/// `__DATA`, in order. Its first initializer, `TEXT`, is [`the_hook`].
fn dev_hello(output: Output) -> Parts {
    let mut parts = hello(output);
    parts.regions[0].capacity = PATCH - TEXT;
    parts.regions.insert(1, region(Class::Patch, PATCH, 4, 0x80, Prot::RX));
    parts.contents.insert(1, vec![0x1f, 0x20, 0x03, 0xd5]);
    let asked = parts.output.dev_run.unwrap_or(DevRun { arena: 0, cells: 0, fixed: 0 });
    let mut at = ARENA;
    for (reserve, len) in [(Reserve::Arena, asked.arena), (Reserve::Cells, asked.cells), (Reserve::Fixed, asked.fixed)] {
        if len != 0 {
            parts.regions.push(Region { class: Class::Reserve(reserve), addr: at, capacity: len, size: 0, prot: Prot::NONE, align: 0x4000 });
            parts.contents.push(Vec::new());
            at += len;
        }
    }
    parts
}

fn the_hook(name: &str) -> Option<u64> {
    (name == super::START).then_some(TEXT)
}

/// Each `LC_SEGMENT_64`: (name, vmaddr, vmsize, fileoff, filesize, maxprot, initprot, nsects).
fn segments_of(file: &[u8]) -> Vec<(String, u64, u64, u64, u64, u32, u32, u32)> {
    use object::read::macho::LoadCommandVariant as V;
    let header = macho::MachHeader64::<LE>::parse(file, 0).unwrap();
    let mut commands = header.load_commands(LE, file, 0).unwrap();
    let mut out = Vec::new();
    while let Some(command) = commands.next().unwrap() {
        if let V::Segment64(it, _) = command.variant().unwrap() {
            let name = name16(it.segname());
            out.push((name, it.vmaddr(LE), it.vmsize(LE), it.fileoff(LE), it.filesize(LE), it.maxprot(LE).0, it.initprot(LE).0, it.nsects(LE)));
        }
    }
    out
}

/// A dev run's file, as the core writes it.
fn dev_file(minimum: u32) -> Vec<u8> {
    let parts = dev_hello(dev_run(minimum));
    let writer = super::writer(&parts.output).unwrap();
    let slices = parts.slices();
    written(&writer, &Placed { symbol: &the_hook, ..parts.placed(&slices) })
}

#[test]
fn a_dev_runs_start_hook_is_the_first_initializer() {
    let parts = dev_hello(dev_run(OPCODES));
    let writer = super::writer(&parts.output).unwrap();
    let (class, rank) = writer.start_hook().unwrap();
    assert_eq!(class, Class::InitArray);
    for name in ["__DATA,__mod_init_func,mod_init_funcs", "__DATA_CONST,__mod_init_func", "__DATA,__mod_init_func"] {
        let (class, input) = writer.section(name, false, false, false).unwrap();
        assert_eq!(class, Class::InitArray, "{name}");
        assert!(rank < input, "{name}: the hook's rank {rank} sorts before {input}");
    }
    for flags in [macho::S_MOD_INIT_FUNC_POINTERS.to_flags().0, macho::S_REGULAR.to_flags().0] {
        let kind = super::classify("__DATA", "__mod_init_func", flags).unwrap();
        let SectionKind::Alloc(Class::InitArray, input) = kind else { panic!("{kind:?}") };
        assert!(rank < input);
    }

    let slices = parts.slices();
    let file = written(&writer, &Placed { symbol: &the_hook, ..parts.placed(&slices) });
    let read = read(&file);
    let init = read.sections.iter().find(|it| it.1 == "__mod_init_func").unwrap();
    assert_eq!((init.0.as_str(), init.2), ("__DATA_CONST", INIT));
    let at = (INIT - BASE) as usize;
    assert_eq!(&file[at..at + 8], &TEXT.to_le_bytes(), "the first slot is the hook");

    let encode_error = |placed: &Placed<'_>| writer.encode(placed).unwrap_err().what;
    let elsewhere = |name: &str| (name == super::START).then_some(TEXT + 4);
    let error = encode_error(&Placed { symbol: &elsewhere, ..parts.placed(&slices) });
    assert!(error.contains("first initializer is 0x100004000"), "{error}");
    let error = encode_error(&parts.placed(&slices));
    assert!(error.contains("doesn't define `_blaze_inproc_start`"), "{error}");
    let mut no_init = dev_hello(dev_run(OPCODES));
    let nth = no_init.regions.iter().position(|it| it.class == Class::InitArray).unwrap();
    no_init.regions.remove(nth);
    no_init.contents.remove(nth);
    no_init.rebase.retain(|it| *it != INIT);
    let slices = no_init.slices();
    let error = encode_error(&Placed { symbol: &the_hook, ..no_init.placed(&slices) });
    assert!(error.contains("no `__mod_init_func`"), "{error}");
}

#[test]
fn a_dev_runs_code_is_alone_on_its_pages_and_never_writable() {
    for minimum in [OPCODES, CHAINED] {
        let file = dev_file(minimum);
        let segments = segments_of(&file);
        let code: Vec<_> = segments.iter().filter(|it| it.6 & macho::VM_PROT_EXECUTE.0 != 0).collect();
        assert_eq!(code.len(), 1);
        for it in code {
            assert_eq!(it.0, "__TEXT");
            assert_eq!((it.1 % 0x4000, it.3 % 0x4000), (0, 0), "on a page in memory and in the file");
            assert_eq!(it.2, it.4, "all file bytes");
            assert_eq!((it.5 | it.6) & macho::VM_PROT_WRITE.0, 0, "never writable, nor able to be made so");
            for other in segments.iter().filter(|other| other.0 != it.0) {
                let memory = other.2 != 0 && other.1 < it.1 + it.2 && it.1 < other.1 + other.2;
                let in_file = other.4 != 0 && other.3 < it.3 + it.4 && it.3 < other.3 + other.4;
                assert!(!memory && !in_file, "`__TEXT` shares a page with {}", other.0);
            }
        }
        let read = read(&file);
        let patch = read.sections.iter().find(|it| it.1 == "__blaze_patch").unwrap();
        assert_eq!((patch.0.as_str(), patch.2), ("__TEXT", PATCH), "live patches' code is code");
        assert_ne!(patch.4 & macho::S_ATTR_PURE_INSTRUCTIONS.0, 0);

        let layout = blaze_inproc::image::layout(blaze_inproc::Format::MachO, &file, 0x4000).unwrap();
        assert_eq!(layout.header, BASE);
        assert_eq!(layout.code, [blaze_inproc::Segment { offset: 0, linked: BASE, len: 0x8000 }]);
        assert_eq!(layout.reserved, [ARENA..CELLS, CELLS..FIXED, FIXED..FIXED + ASKED.fixed]);
    }

    let segment = |name, vmaddr, vmsize, fileoff, filesize, prot| super::SegmentOut {
        name,
        vmaddr,
        vmsize,
        fileoff,
        filesize,
        prot,
        flags: 0,
        sections: Vec::new(),
    };
    let (rx, rw) = (macho::VM_PROT_READ.0 | macho::VM_PROT_EXECUTE.0, macho::VM_PROT_READ.0 | macho::VM_PROT_WRITE.0);
    let text = || segment("__TEXT", BASE, 0x8000, 0, 0x8000, rx);
    let data = |vmaddr, fileoff, filesize| segment("__DATA", vmaddr, 0x4000, fileoff, filesize, rw);
    assert!(super::code_alone(&[text(), data(BASE + 0x8000, 0x8000, 0x10)], 0x4000).is_ok());
    let refused = |segments: &[super::SegmentOut], says: &str| {
        let error = super::code_alone(segments, 0x4000).unwrap_err().what;
        assert!(error.contains(says), "{error} doesn't say {says:?}");
    };
    refused(&[segment("__TEXT", BASE, 0x8000, 0, 0x8000, rx | rw)], "is writable");
    refused(&[segment("__TEXT", BASE + 0x1000, 0x8000, 0x1000, 0x8000, rx)], "does not start on a page");
    refused(&[segment("__TEXT", BASE, 0x8000, 0, 0x6000, rx)], "is not all file bytes");
    refused(&[text(), data(BASE + 0x6000, 0x8000, 0x10)], "shares a page with `__DATA`");
    refused(&[text(), data(BASE + 0x8000, 0x7000, 0x10)], "shares a page with `__DATA`");
}

#[test]
fn a_dev_run_reserves_its_three_ranges_last() {
    let writer = super::writer(&dev_run(OPCODES)).unwrap();
    let reserves = [Class::Reserve(Reserve::Arena), Class::Reserve(Reserve::Cells), Class::Reserve(Reserve::Fixed)];
    let mut classes = vec![reserves[2].clone(), Class::Bss, reserves[0].clone(), Class::Text, reserves[1].clone(), Class::Data];
    writer.order(&mut classes);
    assert_eq!(classes[..3], [Class::Text, Class::Data, Class::Bss]);
    assert_eq!(classes[3..], reserves);
    for class in &reserves {
        assert_eq!(writer.prot(class), Prot::NONE);
    }
    let parts = dev_hello(dev_run(OPCODES));
    let slices = parts.slices();
    let placed = Placed { symbol: &the_hook, ..parts.placed(&slices) };
    for region in parts.regions.iter().filter(|it| matches!(it.class, Class::Reserve(_))) {
        assert_eq!(writer.offset(&placed, region), None, "{:?} is not in the file", region.class);
    }

    for minimum in [OPCODES, CHAINED] {
        let file = dev_file(minimum);
        let segments = segments_of(&file);
        let names: Vec<&str> = segments.iter().map(|it| it.0.as_str()).collect();
        assert_eq!(names, [
            "__PAGEZERO",
            "__TEXT",
            "__DATA_CONST",
            "__DATA",
            "__BLAZE_ARENA",
            "__BLAZE_CELLS",
            "__BLAZE_FIXED",
            "__DWARF",
            "__LINKEDIT"
        ]);
        for (it, (at, len)) in segments[4..7].iter().zip([(ARENA, ASKED.arena), (CELLS, ASKED.cells), (FIXED, ASKED.fixed)]) {
            assert_eq!((it.1, it.2), (at, len), "{}", it.0);
            assert_eq!((it.3, it.4, it.5, it.6, it.7), (0, 0, 0, 0, 0), "{}: no file bytes, no access, no sections", it.0);
        }
        assert!(segments.windows(2).all(|it| it[0].1 + it[0].2 <= it[1].1), "in address order, none overlapping");
        let linkedit = segments.last().unwrap();
        assert_eq!(linkedit.3 + linkedit.4, file.len() as u64, "`__LINKEDIT` last, the signature at its end");
        if minimum == CHAINED {
            let chained = read(&file).chained.unwrap();
            let table = range(&file, chained);
            let starts = u32::from_le_bytes(table[4..8].try_into().unwrap()) as usize;
            let count = u32::from_le_bytes(table[starts..starts + 4].try_into().unwrap()) as usize;
            assert_eq!(count, segments.len(), "a chained start for every segment");
            for nth in 4..7 {
                let at = starts + 4 + 4 * nth;
                assert_eq!(u32::from_le_bytes(table[at..at + 4].try_into().unwrap()), 0, "a reservation has no slots");
            }
        }
    }

    let encode_error = |parts: &Parts| {
        let writer = super::writer(&parts.output).unwrap();
        let slices = parts.slices();
        writer.encode(&Placed { symbol: &the_hook, ..parts.placed(&slices) }).unwrap_err().what
    };
    let mut short = dev_hello(dev_run(OPCODES));
    short.regions.last_mut().unwrap().capacity = 0x4000;
    let error = encode_error(&short);
    assert!(error.contains("`__BLAZE_FIXED` reserves 0x4000 bytes where the dev run asks 0x8000"), "{error}");
    let mut none = dev_hello(dev_run(OPCODES));
    none.output.dev_run = Some(DevRun { fixed: 0, ..ASKED });
    let error = encode_error(&none);
    assert!(error.contains("`__BLAZE_FIXED` reserves 0x8000 bytes where the dev run asks 0x0"), "{error}");
    let mut missing = dev_hello(dev_run(OPCODES));
    missing.output.dev_run = Some(DevRun { arena: 0x20_0000, ..ASKED });
    let error = encode_error(&missing);
    assert!(error.contains("`__BLAZE_ARENA` reserves 0x100000 bytes where the dev run asks 0x200000"), "{error}");
    let mut stray = dev_hello(dev_run(OPCODES));
    stray.output.dev_run = None;
    let error = encode_error(&stray);
    assert!(error.contains("`__BLAZE_ARENA` reserves 0x100000 bytes in an image that is not a dev run"), "{error}");
}

#[test]
fn a_dev_run_is_signed_ad_hoc_without_the_hardened_runtime() {
    for minimum in [OPCODES, CHAINED] {
        let file = dev_file(minimum);
        let (at, len) = read(&file).signature.unwrap();
        let signature = range(&file, (at, len));
        check_signature(signature, &file[..at as usize], "hello");
        let be = |at: usize| u32::from_be_bytes(signature[at..at + 4].try_into().unwrap());
        let flags = be(be(16) as usize + 12);
        assert_ne!(flags & 0x2, 0, "CS_ADHOC");
        assert_eq!(flags & 0x1_0000, 0, "CS_RUNTIME, the hardened runtime, kills the overlaid code");
    }
}

/// `dev_run` adds the reservations and checks; nothing else of the file changes, so an image
/// without one is today's.
#[test]
fn without_a_dev_run_the_image_is_unchanged() {
    for minimum in [OPCODES, CHAINED] {
        let plain = hello(output_at(Arch::Aarch64, OutputKind::Executable { entry: "main".to_owned() }, minimum));
        let mut dev = hello(plain.output.clone());
        dev.output.dev_run = Some(DevRun { arena: 0, cells: 0, fixed: 0 });
        let (plain_slices, dev_slices) = (plain.slices(), dev.slices());
        let without = written(&super::writer(&plain.output).unwrap(), &plain.placed(&plain_slices));
        let with = written(&super::writer(&dev.output).unwrap(), &Placed { symbol: &the_hook, ..dev.placed(&dev_slices) });
        assert_eq!(without, with, "at {minimum:#x}");
        let names: Vec<String> = segments_of(&without).into_iter().map(|it| it.0).collect();
        assert_eq!(names, ["__PAGEZERO", "__TEXT", "__DATA_CONST", "__DATA", "__DWARF", "__LINKEDIT"]);
    }
}

#[test]
fn what_a_dev_run_cannot_overlay_is_an_error() {
    let x86 = Output { arch: Arch::X86_64, ..dev_run(OPCODES) };
    assert!(super::writer(&x86).err().unwrap().what.contains("arm64 executables only"));
    let shared = OutputKind::Shared { name: "@rpath/libx.dylib".to_owned(), exports: Vec::new() };
    let dylib = Output { kind: shared, ..dev_run(OPCODES) };
    assert!(super::writer(&dylib).err().unwrap().what.contains("a dev run of a dynamic library"));

    let mut preinit = hello(executable(Arch::Aarch64));
    preinit.regions[3].class = Class::PreinitArray;
    let writer = super::writer(&preinit.output).unwrap();
    let error = writer.encode(&preinit.placed(&preinit.slices())).unwrap_err().what;
    assert!(error.contains("a `.preinit_array` region at 0x100008010 in a Mach-O image"), "{error}");

    // dyld writes nothing in code, dev run or not.
    let images: [fn() -> Parts; 3] = [
        || hello(executable(Arch::Aarch64)),
        || dev_hello(dev_run(OPCODES)),
        || dev_hello(dev_run(CHAINED)),
    ];
    let encode_error = |parts: &Parts| {
        let writer = super::writer(&parts.output).unwrap();
        let slices = parts.slices();
        writer.encode(&Placed { symbol: &the_hook, ..parts.placed(&slices) }).unwrap_err().what
    };
    for image in images {
        let mut slid = image();
        slid.rebase.insert(0, TEXT + 4);
        let error = encode_error(&slid);
        assert!(
            error.contains("dyld slides a pointer at 0x100004004 in `__ZN5hello6helper17h0123456789abcdefE`, in `__TEXT,__text`"),
            "{error}"
        );
        let mut bound = image();
        bound.binds.insert(0, Bind { addr: TEXT + 0x100, import: 0, addend: 0 });
        let error = encode_error(&bound);
        assert!(error.contains("dyld binds a pointer at 0x100004100 to `_free`, in `__TEXT,__unwind_info`"), "{error}");
        let mut cell = image();
        cell.got.insert(0, GotEntry { addr: TEXT + 0x108, holds: Holds::Import(0) });
        let error = encode_error(&cell);
        assert!(error.contains("dyld fixes up a GOT cell at 0x100004108 to `_free`, in `__TEXT,__unwind_info`"), "{error}");
    }
}

#[test]
fn an_executable_with_the_execution_log_starts_on_its_log_stubs_on_both_arches() {
    for arch in [Arch::Aarch64, Arch::X86_64] {
        let logged = crate::table::linked::logged(output_at(arch, OutputKind::Executable { entry: "main".into() }, CHAINED));
        assert!(logged.header.is_multiple_of(8), "{arch:?}");
    }
}
