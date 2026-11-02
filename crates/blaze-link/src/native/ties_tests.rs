//! Collection of sections kept only through another, over objects built in memory: tied
//! sections (ELF `SHF_LINK_ORDER`, COFF associative COMDATs) and Mach-O's live support.

use blaze_codegen::Arch;

use super::*;
use crate::model::{NativeFixup, Section};
use crate::symbols::IdNo;
use crate::{Format, Output, OutputKind};

const TEXT: Class = Class::Text;

fn counters() -> Class {
    Class::Named { name: "__sancov_cntrs".into(), write: true, exec: false }
}

/// One 16-byte section per entry of `classes`, each with a local function symbol at its
/// start (symbol `i` in section `i`); `refs` are `(section, symbol)` relocations.
fn object(classes: &[Class], refs: &[(u32, u32)]) -> Object {
    let mut names = Names::default();
    let (mut sections, mut symbols, mut fixups) = (Vec::new(), Vec::new(), Vec::new());
    for (at, class) in classes.iter().enumerate() {
        let name = names.push(&format!("s{at}")).unwrap();
        let start = fixups.len() as u32;
        for (k, (_, symbol)) in refs.iter().filter(|it| it.0 == at as u32).enumerate() {
            fixups.push(NativeFixup { at: k as u64, apply: Apply::Abs64, symbol: *symbol, addend: 0, minus: None });
        }
        let what = SectionKind::Alloc(class.clone(), 0);
        sections.push(Section { name, what, align: 1, data: Data::Zero(16), fixups: start..fixups.len() as u32, retain: false });
        let def = SymbolDef::Section { section: at as u32, value: 0, size: 16 };
        symbols.push(Symbol { name, def, binding: Binding::Local, kind: SymbolKind::Function, hidden: false });
    }
    Object { sections, symbols, fixups, names, ..Object::default() }
}

fn tie(section: u32, to: u32) -> Tie {
    Tie { section, to, ordered: true }
}

/// The graph of `objects`, loaded in order, each section's symbol and section its own.
fn graph(objects: Vec<Object>) -> Graph {
    let mut graph = Graph::new();
    for (n, object) in objects.into_iter().enumerate() {
        check_object(&object, 0).unwrap();
        let named: Vec<NameId> = (0..object.sections.len() as u32)
            .filter(|at| matches!(object.sections[*at as usize].what, SectionKind::Alloc(Class::Named { .. }, _)))
            .map(NameId)
            .collect();
        let (symbols, sections) = (object.symbols.len(), object.sections.len() as u32);
        let syms = (0..symbols as u32 + sections).map(Sym).collect();
        let obj = Obj { native: IdNo(n as u32), member: 0 };
        let parsed = Parsed::new(obj, Arc::new(NativeFile::Object(object)), 0, syms, &named, &[]);
        let load = Load {
            parsed: Arc::new(parsed),
            input: n as u32,
            pulled: false,
            sections: graph.sections,
            frames: graph.frames,
            slots: vec![NONE; symbols],
        };
        graph.loads.push(load);
        graph.by_obj.insert(obj, n as u32);
        graph.sections += sections;
    }
    graph.index();
    graph
}

fn output() -> Output {
    Output {
        format: Format::Elf,
        arch: Arch::X86_64,
        kind: OutputKind::Executable { entry: "_start".into() },
        interpreter: Some("/lib64/ld-linux-x86-64.so.2".into()),
        platform: None,
        incremental: false,
        debug: crate::Debug::None,
        live: None,
        tags: 1 << 32,
        windows_subsystem: false,
        dev_run: None,
        exec_log: false,
        sanitize: blaze_codegen::settings::Sanitize::default(),
        rpaths: Vec::new(),
        triple: "x86_64-unknown-linux-gnu".into(),
        stack: None,
    }
}

/// What collection keeps of `graph` from `roots` (`(load, section)`) and the graph's own roots,
/// sorted.
fn kept(graph: &Graph, roots: &[(u32, u32)]) -> Vec<(u32, u32)> {
    let symbols = Symbols::default();
    let writer = crate::model::writer(&output()).unwrap();
    let mut marks = Marks::default();
    let mut marker = Marker::new(graph, &mut marks, &symbols, &writer);
    for at in roots {
        marker.push(*at);
    }
    for flat in &graph.roots {
        marker.push(graph.unflat(*flat));
    }
    marker.run();
    let mut kept: Vec<(u32, u32)> = marker.sections.iter().map(|it| unpack(*it)).collect();
    kept.sort_unstable();
    kept
}

/// ELF and COFF: a live function `s0` and a dead one `s1`, each with counters (`s2`, `s3`)
/// and a PC table (`s4`, `s5`) tied to it; the dead one's PC table also refers to `s6`.
#[test]
fn a_tied_section_is_kept_exactly_when_what_it_is_tied_to_is() {
    let classes = [TEXT, TEXT, counters(), counters(), Class::Rodata, Class::Rodata, TEXT];
    let mut it = object(&classes, &[(4, 0), (5, 1), (5, 6)]);
    it.ties = vec![tie(2, 0), tie(3, 1), tie(4, 0), tie(5, 1)];
    let graph = graph(vec![it]);
    assert_eq!(kept(&graph, &[(0, 0)]), [(0, 0), (0, 2), (0, 4)], "the live function's counters and PCs, nothing of the dead one's");
    assert_eq!(kept(&graph, &[(0, 1)]), [(0, 1), (0, 3), (0, 5), (0, 6)], "a kept PC table's relocations are followed");
    assert_eq!(kept(&graph, &[]), [] as [(u32, u32); 0], "a tied section is no root");
}

#[test]
fn a_tied_section_referred_to_keeps_what_it_is_tied_to() {
    // `s0` refers to `s2`, tied to `s1`: the two are kept together, never one alone.
    let mut it = object(&[TEXT, TEXT, counters()], &[(0, 2)]);
    it.ties = vec![tie(2, 1)];
    let graph = graph(vec![it]);
    assert_eq!(kept(&graph, &[(0, 0)]), [(0, 0), (0, 1), (0, 2)]);
}

#[test]
fn a_tied_section_is_no_root_and_no_start_symbol_keeps_it() {
    let init = Class::InitArray;
    let mut it = object(&[TEXT, init.clone(), init, counters(), counters()], &[]);
    it.ties = vec![tie(1, 0), tie(3, 0)];
    let untied = graph(vec![it]);
    let parsed = &untied.loads[0].parsed;
    assert_eq!(parsed.roots, [2], "the untied init array only");
    assert_eq!(parsed.named, [(NameId(4), 4)], "`__start___sancov_cntrs` keeps only the untied counters");
    assert_eq!(kept(&untied, &[]), [(0, 2)]);

    let mut it = object(&[TEXT, counters()], &[]);
    it.ties = vec![tie(1, 0)];
    it.sections[1].retain = true;
    let retained = graph(vec![it]);
    assert_eq!(retained.loads[0].parsed.roots, [1], "retain says so itself (lld: before SHF_LINK_ORDER)");
    assert_eq!(kept(&retained, &[]), [(0, 0), (0, 1)], "and keeps what it is tied to");
}

/// COFF: an associative COMDAT is in its root's group as well as tied to its parent; a
/// losing group drops the sections tied to its own, and those tied to them.
#[test]
fn a_dropped_section_drops_what_is_tied_to_it() {
    let mut it = object(&[TEXT, counters(), Class::Rodata, TEXT], &[]);
    let name = it.names.push("f").unwrap();
    it.groups = vec![crate::model::Group { name, sections: 0..1, largest: false }];
    it.grouped = vec![0];
    it.ties = vec![Tie { section: 1, to: 0, ordered: false }, Tie { section: 2, to: 1, ordered: false }];
    let mut b = Build::new();
    b.graph = graph(vec![it]);
    b.discard(0, 0);
    let dropped: Vec<bool> = (0..4).map(|at| b.graph.discarded.get(at)).collect();
    assert_eq!(dropped, [true, true, true, false]);
    assert_eq!(kept(&b.graph, &[(0, 1), (0, 2), (0, 3)]), [(0, 3)], "nothing dropped is kept by a reference");
}

/// Mach-O: `s0` live, `s1` and `s6` dead; live support `s2` refers to `s0`, `s3` to `s1`,
/// `s4` to `s0` and `s6`, and `s5` to `s4` only.
#[test]
fn live_support_is_kept_once_something_it_refers_to_is() {
    let classes = [TEXT, TEXT, Class::Rodata, Class::Rodata, Class::Rodata, Class::Rodata, TEXT];
    let mut it = object(&classes, &[(2, 0), (3, 1), (4, 0), (4, 6), (5, 4)]);
    it.live_support = vec![2, 3, 4, 5];
    let graph = graph(vec![it]);
    assert_eq!(graph.loads[0].parsed.roots, [] as [u32; 0]);
    assert_eq!(
        kept(&graph, &[(0, 0)]),
        [(0, 0), (0, 2), (0, 4), (0, 5), (0, 6)],
        "ld64: a kept live support section's references are followed, and what becomes live \
         keeps the live support referring to it"
    );
    assert_eq!(kept(&graph, &[]), [] as [(u32, u32); 0], "live support keeps nothing by itself");
}

#[test]
fn live_support_is_indexed_by_the_graph_s_numbering_of_every_object() {
    // The second object's live support `s1` refers to its own `s0`.
    let first = object(&[TEXT], &[]);
    let mut second = object(&[TEXT, Class::Rodata], &[(1, 0)]);
    second.live_support = vec![1];
    let graph = graph(vec![first, second]);
    assert_eq!(kept(&graph, &[(0, 0)]), [(0, 0)]);
    assert_eq!(kept(&graph, &[(1, 0)]), [(1, 0), (1, 1)]);
}

#[test]
fn ties_and_live_support_are_checked() {
    let base = || object(&[TEXT, counters()], &[]);
    let mut it = base();
    it.ties = vec![tie(1, 2)];
    assert!(check_object(&it, 0).unwrap_err().contains("past the object's 2"));
    let mut it = base();
    it.ties = vec![tie(1, 1)];
    assert!(check_object(&it, 0).unwrap_err().contains("tied to itself"));
    let mut it = base();
    it.ties = vec![tie(1, 0), tie(1, 0)];
    assert!(check_object(&it, 0).unwrap_err().contains("tied twice"));
    let mut it = base();
    it.live_support = vec![2];
    assert!(check_object(&it, 0).unwrap_err().contains("past the object's 2"));
    let mut it = base();
    it.live_support = vec![1, 0];
    assert!(check_object(&it, 0).unwrap_err().contains("not in section order"));
}
