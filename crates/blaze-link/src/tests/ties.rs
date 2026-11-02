//! SanitizerCoverage's tables through a full ELF link: each function's counters and PC table
//! are `SHF_LINK_ORDER` to its section, so a dropped function takes its entries with it, and
//! the two tables stay one entry each per kept function, in the order of the functions.

use blaze_codegen::Target;
use object::write::{Comdat, Object as Builder, Relocation, SectionId, Symbol as Def, SymbolSection};
use object::{
    Architecture, BinaryFormat, ComdatKind, Endianness, RelocationFlags, SectionFlags, SectionKind, SymbolFlags,
    SymbolKind, SymbolScope, elf,
};

use super::{Change, Disk, Image, Linked, Native, NativeKind, TO, function, id, key, name, output, rel32, returns};
use crate::state::What;

/// A function of the object: its name, what it returns, its counters' bytes (one per block),
/// the offset in its code of each block, and whether it is in a COMDAT group with its tables.
struct Covered {
    name: &'static str,
    value: u8,
    counters: &'static [u8],
    blocks: &'static [u64],
    comdat: bool,
}

const A: Covered = Covered { name: "a", value: 1, counters: &[0xa1, 0xa2], blocks: &[0, 4], comdat: false };
const B: Covered = Covered { name: "b", value: 2, counters: &[0xb1], blocks: &[0], comdat: true };
const C: Covered = Covered { name: "c", value: 3, counters: &[0xc1, 0xc2, 0xc3], blocks: &[0, 1, 5], comdat: true };

/// The ELF section header table of `bytes`: each header's offset and name.
fn headers(bytes: &[u8]) -> Vec<(usize, String)> {
    let u16_at = |at: usize| u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap()) as usize;
    let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
    let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap()) as usize;
    let (table, size, count, names) = (u64_at(0x28), u16_at(0x3a), u16_at(0x3c), u16_at(0x3e));
    let strings = u64_at(table + names * size + 24);
    (0..count)
        .map(|index| {
            let at = table + index * size;
            let start = strings + u32_at(at);
            let end = start + bytes[start..].iter().position(|it| *it == 0).unwrap();
            (at, String::from_utf8(bytes[start..end].to_vec()).unwrap())
        })
        .collect()
}

/// Sets the `sh_link` of the `nth` section named `section` to the index of the section `to`.
fn link(bytes: &mut [u8], section: &str, nth: usize, to: &str) {
    let headers = headers(bytes);
    let target = headers.iter().position(|it| it.1 == to).unwrap() as u32;
    let at = headers.iter().filter(|it| it.1 == section).nth(nth).unwrap().0;
    bytes[at + 40..at + 44].copy_from_slice(&target.to_le_bytes());
}

fn flags(b: &mut Builder<'_>, section: SectionId, flags: elf::SectionFlags) {
    b.section_mut(section).flags = SectionFlags::Elf { sh_type: elf::SHT_PROGBITS, sh_flags: flags };
}

/// An x86-64 object: each function in its own `.text.<name>`, returning its value; its
/// counters in a `__sancov_cntrs` and its PC table (`(pc, 1 for the entry block)` pairs) in
/// a `__sancov_pcs`, both `SHF_LINK_ORDER` to its text. The counters are written in the order
/// `counters` gives, the PC tables in `pcs`'s, both by index into `functions`.
fn object(functions: &[Covered], counters: &[usize], pcs: &[usize]) -> Vec<u8> {
    let mut b = Builder::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let mut texts = Vec::new();
    for it in functions {
        let text = b.add_section(Vec::new(), format!(".text.{}", it.name).into_bytes(), SectionKind::Text);
        let at = b.append_section_data(text, &returns(it.value), 16);
        let symbol = b.add_symbol(Def {
            name: it.name.as_bytes().to_vec(),
            value: at,
            size: 6,
            kind: SymbolKind::Text,
            scope: SymbolScope::Dynamic,
            weak: false,
            section: SymbolSection::Section(text),
            flags: SymbolFlags::None,
        });
        texts.push((text, symbol, Vec::new()));
    }
    for &f in counters {
        let section = b.add_section(Vec::new(), b"__sancov_cntrs".to_vec(), SectionKind::Data);
        flags(&mut b, section, elf::SHF_ALLOC | elf::SHF_WRITE | elf::SHF_LINK_ORDER);
        b.append_section_data(section, functions[f].counters, 1);
        texts[f].2.push(section);
    }
    for &f in pcs {
        let section = b.add_section(Vec::new(), b"__sancov_pcs".to_vec(), SectionKind::Data);
        flags(&mut b, section, elf::SHF_ALLOC | elf::SHF_WRITE | elf::SHF_LINK_ORDER);
        for (k, block) in functions[f].blocks.iter().enumerate() {
            let entry = u64::from(k == 0);
            let at = b.append_section_data(section, &[0u64.to_le_bytes(), entry.to_le_bytes()].concat(), 8);
            b.add_relocation(section, Relocation {
                offset: at,
                symbol: texts[f].1,
                addend: *block as i64,
                flags: RelocationFlags::Elf { r_type: elf::R_X86_64_64 },
            })
            .unwrap();
        }
        texts[f].2.push(section);
    }
    for (it, (text, symbol, tables)) in functions.iter().zip(&texts) {
        if it.comdat {
            let sections = [*text].into_iter().chain(tables.iter().copied()).collect();
            b.add_comdat(Comdat { kind: ComdatKind::Any, symbol: *symbol, sections });
        }
    }
    let mut bytes = b.write().unwrap();
    for (nth, &f) in counters.iter().enumerate() {
        link(&mut bytes, "__sancov_cntrs", nth, &format!(".text.{}", functions[f].name));
    }
    for (nth, &f) in pcs.iter().enumerate() {
        link(&mut bytes, "__sancov_pcs", nth, &format!(".text.{}", functions[f].name));
    }
    bytes
}

/// `call` of each of `calls`, then `lea` of the bounds of the counters and of the PC table.
fn start(calls: &[&str]) -> blaze_codegen::CompiledFunction {
    let mut code = Vec::new();
    let mut relocs = Vec::new();
    for it in calls {
        relocs.push(rel32(code.len() as u32 + 1, Target::Extern((*it).into())));
        code.extend_from_slice(&[0xe8, 0, 0, 0, 0]);
    }
    for it in ["__start___sancov_cntrs", "__stop___sancov_cntrs", "__start___sancov_pcs", "__stop___sancov_pcs"] {
        relocs.push(rel32(code.len() as u32 + 3, Target::Extern(it.into())));
        code.extend_from_slice(&[0x48, 0x8d, 0x05, 0, 0, 0, 0]);
    }
    code.push(0xc3);
    function(1, &code, relocs)
}

/// What a full link of `object` and a `_start` calling `calls` gives: the counters' bytes, the
/// PC table's `(pc, flags)` entries, each called function's address, and how many native
/// sections the image has.
fn linked(object: &std::sync::Arc<[u8]>, calls: &[&str]) -> (Vec<u8>, Vec<(u64, u64)>, Vec<u64>, usize) {
    let start = start(calls);
    let mut image = Image::new(name(TO), output(false)).unwrap();
    let mut disk = Disk::default();
    let change = Change {
        added: vec![&start],
        names: vec![("_start".into(), Target::Direct(key(1)))],
        debug_names: vec![(key(1), "start".into())],
        natives: vec![Native { id: id(80), name: "covered.o", bytes: object, kind: NativeKind::Object, order: 0 }],
        ..Change::default()
    };
    disk.link(&mut image, change).unwrap();
    let natives = image.state.live_pieces().filter(|it| matches!(it.what(), What::Native { .. })).count();
    let out = Linked::of(disk.file(TO));
    let s = out.symbol("start");
    let called: Vec<u64> = (0..calls.len() as u64).map(|it| out.rel32(s + 5 * it + 1)).collect();
    let bounds = s + 5 * calls.len() as u64 + 3;
    let field = |k: u64| out.rel32(bounds + 7 * k);
    let (counters, counters_end, pcs, pcs_end) = (field(0), field(1), field(2), field(3));
    let counter_bytes = out.at(counters, (counters_end - counters) as usize).to_vec();
    let entries = (pcs..pcs_end).step_by(16).map(|at| (out.u64_at(at), out.u64_at(at + 8))).collect();
    (counter_bytes, entries, called, natives)
}

/// Of each function of `functions` at its address in `called`, its blocks' PC table entries.
fn entries(functions: &[(&Covered, u64)]) -> Vec<(u64, u64)> {
    functions
        .iter()
        .flat_map(|(it, at)| it.blocks.iter().enumerate().map(move |(k, block)| (at + block, u64::from(k == 0))))
        .collect()
}

#[test]
fn a_dropped_function_s_counters_and_pcs_go_with_it_and_the_rest_keep_its_order() {
    // Counters written c, b, a and PC tables c, a, b: the image has them in the order of the
    // functions' sections, a then c, not their own.
    let object: std::sync::Arc<[u8]> = object(&[A, B, C], &[2, 1, 0], &[2, 0, 1]).into();
    let (counters, pcs, called, natives) = linked(&object, &["a", "c"]);
    let (a, c) = (called[0], called[1]);
    assert_eq!(counters, [A.counters, C.counters].concat(), "b's counter is gone; a's come first");
    assert_eq!(pcs, entries(&[(&A, a), (&C, c)]), "b's PC entry is gone; one per counter, in the counters' order");
    assert_eq!(pcs.len(), counters.len());
    assert_eq!(natives, 6, "a and c with their tables: `__start_` keeps no tied section, and b's PCs keep no b");
}

#[test]
fn every_function_kept_keeps_every_entry_in_the_functions_order() {
    let object: std::sync::Arc<[u8]> = object(&[A, B, C], &[2, 1, 0], &[1, 2, 0]).into();
    let (counters, pcs, called, natives) = linked(&object, &["c", "a", "b"]);
    let (c, a, b) = (called[0], called[1], called[2]);
    assert_eq!(counters, [A.counters, B.counters, C.counters].concat());
    assert_eq!(pcs, entries(&[(&A, a), (&B, b), (&C, c)]));
    assert_eq!(natives, 9);
}

#[test]
fn a_tied_section_keeps_nothing_alive_by_itself() {
    let object: std::sync::Arc<[u8]> = object(&[A, B, C], &[0, 1, 2], &[0, 1, 2]).into();
    let (counters, pcs, called, natives) = linked(&object, &["b"]);
    assert_eq!(counters, B.counters);
    assert_eq!(pcs, entries(&[(&B, called[0])]));
    assert_eq!(natives, 3, "neither a's nor c's PC table, which refer to them, keeps a or c");
}

#[test]
fn link_order_to_a_section_the_link_leaves_out_is_an_error() {
    let mut b = Builder::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
    let text = b.add_section(Vec::new(), b".text.a".to_vec(), SectionKind::Text);
    b.append_section_data(text, &returns(1), 16);
    let debug = b.add_section(Vec::new(), b".debug_a".to_vec(), SectionKind::Debug);
    b.append_section_data(debug, &[0; 4], 1);
    let counters = b.add_section(Vec::new(), b"__sancov_cntrs".to_vec(), SectionKind::Data);
    flags(&mut b, counters, elf::SHF_ALLOC | elf::SHF_WRITE | elf::SHF_LINK_ORDER);
    b.append_section_data(counters, &[0], 1);
    let mut bytes = b.write().unwrap();
    link(&mut bytes, "__sancov_cntrs", 0, ".debug_a");
    let bytes: std::sync::Arc<[u8]> = bytes.into();
    let start = function(1, &returns(0), Vec::new());
    let mut image = Image::new(name(TO), output(false)).unwrap();
    let change = Change {
        added: vec![&start],
        names: vec![("_start".into(), Target::Direct(key(1)))],
        natives: vec![Native { id: id(81), name: "bad.o", bytes: &bytes, kind: NativeKind::Object, order: 0 }],
        ..Change::default()
    };
    let error = image.stage(change).unwrap_err();
    assert!(error.what.contains("SHF_LINK_ORDER") && error.what.contains("bad.o"), "{error}");
}
