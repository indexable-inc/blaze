//! The schema's promises: each record reads back what was written, two builders of one record
//! write equal bytes whatever order they are given rows in, a scope answers by name what it
//! lists, and the two traits stop by type (`NoItem`, `NotBuilt`, `WrongCrate`, `NoLocal`).

use std::alloc::Global;

use blaze_format::Aligned;
use blaze_id::{ContentId, CrateItemKey, ItemKey, NoItem};
use blaze_sim::{Broke, Config, Tape};

use crate::testing::{Fact, MemSurface};
use crate::{
    AnyItem, Binding, ByteRange, CrateGraph, CrateGraphView, CrateRoot, CrateRootHead,
    CrateRootView, DeclaredName, Edition, ExportClass, FileItem, FileItems, FileItemsView, HasText,
    ItemIndex, ItemKind, Items, KeyEntry, KeyIndex, KeyIndexView, LangItems, LangItemsView,
    LocalTable, MacroCall, MacroDef, MacroDefHead, MacroDefView, MacroKind, ModDecl, ModuleDecls,
    ModuleDeclsView, ModuleFile, ModuleFileView, ModuleScope, Namespace, NoImplsOf, NoImplsOfView,
    NotBuilt, Origin, Place, ScopeView, Source, SurfaceError, Target, TextualEntry,
    TextualEntryView, UseLeaf, Visibility,
};

/// The crate every test's items are of.
const KRATE: CrateItemKey = CrateItemKey(ContentId([1; 32]));

/// An item of [`KRATE`]: its crate's 8 bytes, then `n`.
fn key(n: u8) -> ItemKey {
    let mut bytes = [n; 32];
    bytes[..8].copy_from_slice(&KRATE.prefix());
    ItemKey(ContentId(bytes))
}

/// The content of a file.
fn file(n: u8) -> ContentId {
    ContentId([0xf0 ^ n; 32])
}

/// Few names, so draws repeat them.
const NAMES: [&str; 8] = ["a", "b", "main", "Vec", "é", "r#fn", "_", "zz"];
const KINDS: [ItemKind; 6] =
    [ItemKind::Function, ItemKind::Struct, ItemKind::Module, ItemKind::Impl, ItemKind::Use, ItemKind::Field];
const SPACES: [Namespace; 3] = [Namespace::Types, Namespace::Values, Namespace::Macros];

fn name(tape: &Tape) -> &'static str {
    tape.pick(&NAMES, "name").copied().expect("NAMES is not empty")
}

fn small(tape: &Tape, below: u32, label: &'static str) -> u32 {
    u32::try_from(tape.below(u64::from(below), label)).expect("a draw below a u32 fits one")
}

/// A range inside an item `len` bytes long.
fn range(tape: &Tape, len: u32) -> ByteRange {
    let start = small(tape, len + 1, "range start");
    ByteRange { start, len: small(tape, len - start + 1, "range len") }
}

/// The rows of one file, drawn: items in order, each parent before it, and `mod`, `use` and
/// macro-call rows of them.
struct DrawnFile {
    items: Vec<FileItem<'static>, Global>,
    mods: Vec<ModDecl<'static>, Global>,
    uses: Vec<UseLeaf<'static>, Global>,
    calls: Vec<MacroCall<'static>, Global>,
}

fn drawn_file(tape: &Tape) -> DrawnFile {
    let mut at = 0;
    let mut items = Vec::new_in(Global);
    for index in 0..small(tape, 12, "items") {
        let len = small(tape, 200, "len");
        let parent = match index > 0 && tape.coin("has a parent") {
            true => Some(ItemIndex(small(tape, index, "parent"))),
            false => None,
        };
        items.push(FileItem {
            kind: *tape.pick(&KINDS, "kind").expect("KINDS is not empty"),
            name: name(tape),
            at,
            len,
            signature: range(tape, len),
            body: range(tape, len),
            attrs: range(tape, len),
            parent,
        });
        at += len;
    }
    let (mut mods, mut uses, mut calls) = (Vec::new_in(Global), Vec::new_in(Global), Vec::new_in(Global));
    let count = u32::try_from(items.len()).expect("at most 12 items");
    if count > 0 {
        for _ in 0..small(tape, 4, "rows") {
            let item = ItemIndex(small(tape, count, "of item"));
            match tape.below(3, "row") {
                0 => mods.push(ModDecl { name: name(tape), item, inline: tape.coin("inline") }),
                1 => uses.push(UseLeaf {
                    path: name(tape),
                    alias: tape.coin("aliased").then(|| name(tape)),
                    glob: tape.coin("glob"),
                    item,
                }),
                _ => calls.push(MacroCall {
                    path: name(tape),
                    at: small(tape, 1000, "call at"),
                    len: small(tape, 100, "call len"),
                    parent: tape.coin("inside").then_some(item),
                }),
            }
        }
    }
    DrawnFile { items, mods, uses, calls }
}

fn write_file(drawn: &DrawnFile, mods_first: bool) -> Vec<u8, Global> {
    let mut out = FileItems::new_in(file(0), Global);
    for item in &drawn.items {
        out.item(*item).expect("a drawn item is valid");
    }
    let leaves = |out: &mut FileItems<Global>| {
        for leaf in &drawn.uses {
            out.use_leaf(*leaf).expect("a drawn leaf is valid");
        }
        for call in &drawn.calls {
            out.macro_call(*call).expect("a drawn call is valid");
        }
    };
    if !mods_first {
        leaves(&mut out);
    }
    for module in &drawn.mods {
        out.module(*module).expect("a drawn mod is valid");
    }
    if mods_first {
        leaves(&mut out);
    }
    out.finish().expect("a drawn file is small")
}

/// The refinement: a file's items read back as written, row for row, and the order the kinds
/// of row are given in moves no byte.
#[test]
fn file_items_read_back_as_written() {
    blaze_sim::check(Config::cases(300), |tape| {
        let drawn = drawn_file(tape);
        let bytes = write_file(&drawn, true);
        if bytes != write_file(&drawn, false) {
            return Err(Broke::Oracle("two orders of one file's rows wrote different bytes".into()));
        }
        let buffer = Aligned::copy_from(&bytes);
        let view = FileItemsView::read(&buffer).map_err(|error| Broke::Oracle(error.to_string()))?;
        let same = view.file() == file(0)
            && view.items().eq(drawn.items.iter().copied())
            && view.mods().eq(drawn.mods.iter().copied())
            && view.uses().eq(drawn.uses.iter().copied())
            && view.calls().eq(drawn.calls.iter().copied());
        match same {
            true => Ok(()),
            false => Err(Broke::Oracle("a file's items read back other than written".into())),
        }
    });
}

#[test]
fn a_parent_after_its_child_and_a_range_past_the_item_are_refused() {
    let mut out = FileItems::new_in(file(0), Global);
    let item = FileItem {
        kind: ItemKind::Function,
        name: "f",
        at: 0,
        len: 10,
        signature: ByteRange { start: 0, len: 4 },
        body: ByteRange { start: 4, len: 7 },
        attrs: ByteRange::EMPTY,
        parent: None,
    };
    assert_eq!(out.item(item), Err(crate::BuildError::PastItem { index: 0 }));
    let item = FileItem { body: ByteRange { start: 4, len: 6 }, parent: Some(ItemIndex(0)), ..item };
    assert!(matches!(out.item(item), Err(crate::BuildError::BadIndex { index: 0, .. })));
}

/// A drawn scope: at most one binding per (namespace, name), in a drawn order.
fn bindings(tape: &Tape) -> Vec<Binding<'static>, Global> {
    let mut out = Vec::new_in(Global);
    for ns in SPACES {
        for name in NAMES {
            if !tape.coin("bound") {
                continue;
            }
            let n = u8::try_from(tape.below(200, "item")).expect("below 200 fits a u8");
            let target = match tape.below(3, "state") {
                0 => Target::Item(key(n)),
                1 => Target::Undetermined,
                _ => Target::Ambiguous(key(n), key(n ^ 1)),
            };
            let vis = match tape.below(4, "vis") {
                0 => Visibility::Pub,
                1 => Visibility::Crate,
                2 => Visibility::Restricted(key(250)),
                _ => Visibility::Private,
            };
            let origin = [Origin::Declared, Origin::Import, Origin::Glob, Origin::Expanded]
                [usize::try_from(tape.below(4, "origin")).expect("below 4")];
            out.push(Binding { ns, name, target, vis, origin });
        }
    }
    let turn = usize::try_from(tape.below(u64::try_from(out.len() + 1).expect("few"), "turn")).expect("few");
    let turn = turn.min(out.len());
    out.rotate_left(turn);
    out
}

/// A bug planted in a scope's builder, which the order oracle must catch.
#[derive(Clone, Copy, Debug)]
enum Plant {
    None,
    /// The builder's sort is skipped: rows land in the order given.
    Unsorted,
}

fn write_scope(rows: &[Binding<'_>], plant: Plant) -> Vec<u8, Global> {
    let mut out = ModuleScope::new_in(key(200), Global);
    out.unsorted = matches!(plant, Plant::Unsorted);
    for row in rows {
        out.bind(*row).expect("a drawn binding is valid");
    }
    out.fellow(key(202));
    out.fellow(key(201));
    out.finish().expect("a drawn scope holds one binding per slot")
}

/// Two builders of one scope, given its bindings in two orders, write equal bytes; and the
/// scope answers each binding by its name.
fn scope_agrees(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let rows = bindings(tape);
    let mut reversed = Vec::with_capacity_in(rows.len(), Global);
    reversed.extend(rows.iter().rev().copied());
    let bytes = write_scope(&rows, plant);
    if bytes != write_scope(&reversed, plant) {
        return Err(Broke::Oracle("two builders of one scope wrote different bytes".into()));
    }
    let buffer = Aligned::copy_from(&bytes);
    let view = ScopeView::read(&buffer).map_err(|error| Broke::Oracle(error.to_string()))?;
    for row in &rows {
        if view.get(row.ns, row.name) != Some(*row) {
            return Err(Broke::Oracle(format!("the scope does not answer {row:?} by its name")));
        }
    }
    if view.bindings().count() != rows.len() || !view.fellows().eq([key(201), key(202)]) {
        return Err(Broke::Oracle("the scope lists other bindings or fellows than written".into()));
    }
    Ok(())
}

/// The invariant: a scope's bytes are a function of its bindings, not of their order, and
/// each binding is found by its namespace and name.
#[test]
fn a_scope_is_its_bindings_in_any_order() {
    blaze_sim::check(Config::cases(300), |tape| scope_agrees(tape, Plant::None));
}

/// The control: a builder that keeps the order it was given writes different bytes for two
/// orders, and the oracle must say so.
#[test]
fn the_oracle_catches_an_unsorted_scope() {
    blaze_sim::control(
        Config::cases(300),
        Plant::None,
        Plant::Unsorted,
        "wrote different bytes",
        scope_agrees,
    );
}

#[test]
fn an_export_digest_moves_only_with_its_class() {
    let public = Binding {
        ns: Namespace::Values,
        name: "f",
        target: Target::Item(key(3)),
        vis: Visibility::Pub,
        origin: Origin::Declared,
    };
    let private = Binding { name: "g", target: Target::Item(key(4)), vis: Visibility::Private, ..public };
    let read = |rows: &[Binding<'_>]| {
        let buffer = Aligned::copy_from(&write_scope(rows, Plant::None));
        let view = ScopeView::read(&buffer).expect("a written scope reads");
        [ExportClass::Pub, ExportClass::Crate, ExportClass::Within].map(|class| view.exports(class))
    };
    let [pub_before, crate_before, within_before] = read(&[public]);
    let [pub_after, crate_after, within_after] = read(&[public, private]);
    assert_eq!((pub_before, crate_before), (pub_after, crate_after));
    assert_ne!(within_before, within_after);
}

/// The refinement: a key index page answers each key with its entry, in any order given.
#[test]
fn a_key_index_answers_each_key_in_one_probe() {
    blaze_sim::check(Config::cases(300), |tape| {
        let mut entries = Vec::new_in(Global);
        for n in 0..u8::try_from(tape.below(24, "keys")).expect("below 24") {
            let index = ItemIndex(small(tape, 50, "index"));
            let place = match tape.coin("expanded") {
                false => Place::File { file: file(n % 3), index },
                true => Place::Expansion { expansion: ContentId([n; 32]), index },
            };
            let parent = tape.coin("has a parent").then(|| key(n.wrapping_add(100)));
            entries.push(KeyEntry { key: key(n), place, parent });
        }
        let turn = usize::try_from(tape.below(25, "turn")).expect("below 25").min(entries.len());
        let write = |rows: &[KeyEntry]| {
            let mut out = KeyIndex::new_in(key(255), Global);
            rows.iter().for_each(|row| out.entry(*row));
            out.finish().expect("distinct keys")
        };
        let bytes = write(&entries);
        entries.rotate_left(turn);
        if bytes != write(&entries) {
            return Err(Broke::Oracle("two orders of one page wrote different bytes".into()));
        }
        let buffer = Aligned::copy_from(&bytes);
        let view = KeyIndexView::read(&buffer).map_err(|error| Broke::Oracle(error.to_string()))?;
        let all = entries.iter().all(|entry| view.get(entry.key) == Some(*entry));
        match all && view.get(key(99)).is_none() && view.entries().count() == entries.len() {
            true => Ok(()),
            false => Err(Broke::Oracle("the page answers a key other than written".into())),
        }
    });
}

/// One module, `key(200)`, of one file, `file(0)`: `fn a() {}` and a struct a macro made.
fn surface(rows: &[Binding<'_>]) -> MemSurface {
    let mut surface = MemSurface::new(KRATE);
    let mut items = FileItems::new_in(file(0), Global);
    items
        .item(FileItem {
            kind: ItemKind::Function,
            name: "a",
            at: 0,
            len: 9,
            signature: ByteRange { start: 0, len: 6 },
            body: ByteRange { start: 7, len: 2 },
            attrs: ByteRange::EMPTY,
            parent: None,
        })
        .expect("a valid item");
    surface.insert(Fact::FileItems, &items.finish().expect("small")).expect("a written record");
    surface.insert_text(file(0), "fn a() {}");
    let mut page = KeyIndex::new_in(key(200), Global);
    page.entry(KeyEntry {
        key: key(1),
        place: Place::File { file: file(0), index: ItemIndex(0) },
        parent: Some(key(200)),
    });
    page.entry(KeyEntry {
        key: key(2),
        place: Place::Expansion { expansion: ContentId([9; 32]), index: ItemIndex(0) },
        parent: Some(key(200)),
    });
    surface.insert(Fact::KeyIndex, &page.finish().expect("distinct keys")).expect("a written record");
    surface.insert(Fact::ModuleScope, &write_scope(rows, Plant::None)).expect("a written record");
    surface
}

/// The refinement: through `Items`, `lookup` finds exactly what `scope` lists.
#[test]
fn lookup_finds_what_scope_lists() {
    blaze_sim::check(Config::cases(200), |tape| {
        let rows = bindings(tape);
        let surface = surface(&rows);
        let module = key(200);
        let scope = surface.scope(module).map_err(|error| Broke::Oracle(error.to_string()))?;
        for binding in scope.bindings() {
            let found = surface.lookup(module, binding.ns, binding.name);
            if found != Ok(Some(binding.target)) {
                return Err(Broke::Oracle(format!("lookup of {binding:?} gave {found:?}")));
            }
        }
        for ns in SPACES {
            for name in NAMES {
                let listed = scope.get(ns, name).map(|binding| binding.target);
                if surface.lookup(module, ns, name) != Ok(listed) {
                    return Err(Broke::Oracle(format!("lookup of {name} in {ns} is not what scope lists")));
                }
            }
        }
        Ok(())
    });
}

#[test]
fn an_unknown_item_is_no_item() {
    let surface = surface(&[]);
    let missing = SurfaceError::NoItem(NoItem { krate: KRATE, key: key(77) });
    assert_eq!(surface.kind(key(77)), Err(missing));
    let other = ItemKey(ContentId([3; 32]));
    assert_eq!(surface.kind(other), Err(SurfaceError::NoItem(NoItem { krate: KRATE, key: other })));
    assert_eq!(
        surface.decls(key(200)).map(|view| view.module()),
        Err(SurfaceError::NoRecord { fact: "ModuleDecls", item: key(200) })
    );
}

#[test]
fn an_item_a_macro_made_is_not_built() {
    let surface = surface(&[]);
    let stop = NotBuilt { method: "kind", item: key(2) };
    assert_eq!(surface.kind(key(2)), Err(SurfaceError::NotBuilt(stop)));
    assert_eq!(
        stop.to_string(),
        "not built: kind of 0101010101010101020202020202020202020202020202020202020202020202"
    );
    assert_eq!(surface.parent(key(2)), Ok(Some(key(200))));
}

#[test]
fn source_reads_an_items_text_only_with_its_crates_proof() {
    let surface = surface(&[]);
    // SAFETY: this test is the driver that opened the crate's one file: it inserted its text.
    let proof = unsafe { HasText::assume(KRATE) };
    assert_eq!(surface.kind(key(1)), Ok(ItemKind::Function));
    assert_eq!(surface.signature_text(proof, key(1)), Ok("fn a()"));
    assert_eq!(surface.body_text(proof, key(1)), Ok("{}"));
    let other = CrateItemKey(ContentId([5; 32]));
    // SAFETY: as above; the proof names another crate on purpose, to be refused.
    let wrong = unsafe { HasText::assume(other) };
    assert_eq!(surface.text(wrong, file(0)), Err(SurfaceError::WrongCrate { proof: other, krate: KRATE }));
}

#[test]
fn a_local_is_its_key_within_one_task() {
    let rows = [Binding {
        ns: Namespace::Values,
        name: "a",
        target: Target::Item(key(1)),
        vis: Visibility::Pub,
        origin: Origin::Declared,
    }];
    let surface = surface(&rows);
    let mut locals: LocalTable<AnyItem, Global> = LocalTable::new_in(Global);
    let module = locals.intern(key(200)).expect("room");
    assert_eq!(locals.intern(key(200)), Ok(module));
    let found = surface.local_lookup(&mut locals, module, Namespace::Values, "a").expect("bound");
    let Some(Target::Item(item)) = found else { panic!("`a` is an item: {found:?}") };
    assert_eq!(locals.key(item), Ok(key(1)));
    assert_eq!(surface.local_kind(&locals, item), Ok(ItemKind::Function));
    assert_eq!(surface.local_parent(&mut locals, item), Ok(Some(module)));
    let mut other: LocalTable<AnyItem, Global> = LocalTable::new_in(Global);
    let foreign = other.intern(key(1)).and_then(|_| other.intern(key(2))).expect("room");
    assert_eq!(locals.len(), 2);
    assert_eq!(surface.local_kind(&locals, foreign), Ok(ItemKind::Function));
    let mut third: LocalTable<AnyItem, Global> = LocalTable::new_in(Global);
    let far = (0..5).map(|n| third.intern(key(n + 10))).last().expect("five").expect("room");
    assert_eq!(locals.key(far), Err(SurfaceError::NoLocal { local: 4 }));
}

#[test]
fn module_decls_are_sorted_and_found_by_name() {
    let names = [
        DeclaredName { ns: Namespace::Values, name: "b", item: key(2), kind: ItemKind::Function, vis: Visibility::Pub },
        DeclaredName { ns: Namespace::Types, name: "a", item: key(1), kind: ItemKind::Struct, vis: Visibility::Private },
        DeclaredName { ns: Namespace::Values, name: "a", item: key(1), kind: ItemKind::Struct, vis: Visibility::Private },
    ];
    let write = |rows: &[DeclaredName<'_>]| {
        let mut out = ModuleDecls::new_in(key(200), Global);
        rows.iter().for_each(|row| out.declare(*row).expect("small"));
        out.finish().expect("small")
    };
    let mut reversed = names;
    reversed.reverse();
    let bytes = write(&names);
    assert_eq!(bytes, write(&reversed));
    let buffer = Aligned::copy_from(&bytes);
    let view = ModuleDeclsView::read(&buffer).expect("a written record");
    assert!(view.names().eq([names[1], names[2], names[0]]));
    assert!(view.named(Namespace::Values, "a").eq([names[2]]));
}

#[test]
fn a_textual_entry_answers_the_last_definition_of_a_name() {
    let mut out = TextualEntry::new_in(key(200), Global);
    out.push("m", key(1)).expect("small");
    out.push("n", key(2)).expect("small");
    out.push("m", key(3)).expect("small");
    let buffer = Aligned::copy_from(&out.finish().expect("small"));
    let view = TextualEntryView::read(&buffer).expect("a written record");
    assert_eq!((view.get("m"), view.get("n"), view.get("o")), (Some(key(3)), Some(key(2)), None));
    assert_eq!(view.macros().count(), 3);
}

#[test]
fn a_module_file_reads_back_with_its_absent_paths_sorted() {
    let mut out = ModuleFile::new_in(key(200), file(1), "src/a.rs", None, Global).expect("small");
    out.absent("src/a/mod.rs").expect("small");
    out.absent("src/a.rs.in").expect("small");
    out.absent("src/a/mod.rs").expect("small");
    let buffer = Aligned::copy_from(&out.finish().expect("small"));
    let view = ModuleFileView::read(&buffer).expect("a written record");
    assert_eq!((view.module(), view.file(), view.path(), view.inline()), (key(200), file(1), "src/a.rs", None));
    assert!(view.absent().eq(["src/a.rs.in", "src/a/mod.rs"]));
}

#[test]
fn a_crate_root_reads_back_sorted_and_refuses_two_crates_of_one_name() {
    let head = CrateRootHead {
        krate: KRATE,
        root_module: key(200),
        root_file: file(0),
        edition: Edition::E2024,
        no_std: true,
        no_core: false,
        prelude: Some(key(201)),
    };
    let core = CrateItemKey(ContentId([7; 32]));
    let alloc = CrateItemKey(ContentId([8; 32]));
    let mut out = CrateRoot::new_in(head, Global);
    out.extern_crate("core", core).expect("small");
    out.extern_crate("alloc", alloc).expect("small");
    out.self_name("me").expect("small");
    out.tool("blaze").expect("small");
    out.tool("blaze").expect("small");
    let buffer = Aligned::copy_from(&out.finish().expect("small"));
    let view = CrateRootView::read(&buffer).expect("a written record");
    assert_eq!(view.head(), head);
    assert!(view.externs().eq([("alloc", alloc), ("core", core)]));
    assert_eq!((view.extern_crate("core"), view.extern_crate("std")), (Some(core), None));
    assert!(view.self_names().eq(["me"]) && view.tools().eq(["blaze"]));
    let mut twice = CrateRoot::new_in(head, Global);
    twice.extern_crate("core", core).expect("small");
    twice.extern_crate("core", alloc).expect("small");
    assert!(matches!(twice.finish().err(), Some(crate::BuildError::Twice { .. })));
}

#[test]
fn a_macro_def_reads_back_with_its_helpers_in_order() {
    let head = MacroDefHead {
        item: key(5),
        name: "Serialize",
        body: ContentId([6; 32]),
        place: Some(Place::File { file: file(2), index: ItemIndex(3) }),
        kind: MacroKind::ProcDerive,
        exported: false,
        local_inner: false,
    };
    let mut out = MacroDef::new_in(head, Global);
    out.helper("serde").expect("small");
    out.helper("allow").expect("small");
    let buffer = Aligned::copy_from(&out.finish().expect("small"));
    let view = MacroDefView::read(&buffer).expect("a written record");
    assert_eq!(view.head(), head);
    assert!(view.helpers().eq(["serde", "allow"]));
    let builtin = MacroDefHead { place: None, kind: MacroKind::Builtin, ..head };
    let buffer = Aligned::copy_from(&MacroDef::new_in(builtin, Global).finish().expect("small"));
    assert_eq!(MacroDefView::read(&buffer).expect("a written record").head(), builtin);
}

#[test]
fn lang_items_a_crate_graph_and_impls_read_back() {
    let mut lang = LangItems::new_in(KRATE, Global);
    lang.item("sized", key(1)).expect("small");
    lang.item("copy", key(2)).expect("small");
    let buffer = Aligned::copy_from(&lang.finish().expect("distinct names"));
    let view = LangItemsView::read(&buffer).expect("a written record");
    assert_eq!((view.get("copy"), view.get("sized"), view.get("drop")), (Some(key(2)), Some(key(1)), None));

    let (core, std) = (CrateItemKey(ContentId([7; 32])), CrateItemKey(ContentId([9; 32])));
    let mut graph = CrateGraph::new_in(KRATE, Global);
    graph.krate(std, "std").expect("small");
    graph.krate(KRATE, "app").expect("small");
    graph.krate(core, "core").expect("small");
    graph.dep(KRATE, std, "std").expect("small");
    graph.dep(KRATE, core, "core").expect("small");
    graph.dep(std, core, "core").expect("small");
    let buffer = Aligned::copy_from(&graph.finish().expect("a closed graph"));
    let view = CrateGraphView::read(&buffer).expect("a written record");
    assert!(view.crates().eq([(KRATE, "app"), (core, "core"), (std, "std")]));
    assert!(view.deps_of(KRATE).eq([("core", core), ("std", std)]));
    assert_eq!((view.dep(std, "core"), view.dep(core, "std")), (Some(core), None));
    let mut open = CrateGraph::new_in(KRATE, Global);
    open.krate(KRATE, "app").expect("small");
    open.dep(KRATE, core, "core").expect("small");
    assert!(matches!(open.finish().err(), Some(crate::BuildError::Unknown { .. })));

    let mut impls = NoImplsOf::new_in(KRATE, Global);
    impls.implements(key(9));
    impls.implements(key(4));
    impls.implements(key(9));
    let buffer = Aligned::copy_from(&impls.finish().expect("small"));
    let view = NoImplsOfView::read(&buffer).expect("a written record");
    assert!(!view.has_no_impl_of(key(4)) && !view.has_no_impl_of(key(9)) && view.has_no_impl_of(key(5)));
}
