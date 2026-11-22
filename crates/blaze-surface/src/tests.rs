//! The schema's promises: each record reads back what was written, two builders of one record
//! write equal bytes whatever order they are given rows in, a scope answers by name what it
//! lists, and the two traits stop by type (`NoItem`, `NotBuilt`, `WrongCrate`, `NoLocal`).

use blaze_format::Aligned;
use blaze_id::{ContentId, CrateItemKey, ItemKey, NoItem};
use blaze_sim::{Broke, Config, Tape};

use crate::testing::{Fact, MemSurface};
use crate::{
    AnyItem, Binding, BindingState, Def, DefKind, Edition, FileItems, HasText, ItemIndex,
    ItemKind, Items, KeyEntry, KeyIndex, KeyIndexView, LocalTable, MacroDef, MacroDefHead,
    MacroDefView, MacroKind, ModuleFile, ModuleFileView, Namespace, NotBuilt, Origin, Place,
    ScopeEntry, Source, SurfaceError, TextualEntry, TextualEntryView, Visibility,
};
use crate::{BuiltinDerive, DefSite, ProcKind, Transparency};

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
const SPACES: [Namespace; 3] = [Namespace::Types, Namespace::Values, Namespace::Macros];

fn name(tape: &Tape) -> &'static str {
    tape.pick(&NAMES, "name").copied().expect("NAMES is not empty")
}

fn small(tape: &Tape, below: u32, label: &'static str) -> u32 {
    u32::try_from(tape.below(u64::from(below), label)).expect("a draw below a u32 fits one")
}

/// `FileItems`' promises: in the module below, beside the record's own types.
mod file_items;

/// The name-resolution records' promises (`ModuleDecls`, `ModuleScope`, `CrateRoot`), and the
/// drawn scopes the trait tests below read through.
mod scope_records;

use scope_records::{Plant, bindings, write_scope};

/// The tool namespaces and the `blaze::` table: sorted and unique (`classify` searches it), every
/// name of the ledger known, and an unknown name answered with its nearest within distance 2.
mod tool_attrs {
    use crate::tool_attrs::{BLAZE, BlazeAttr, KNOWN, State, TOOLS, classify};

    #[test]
    fn tables_are_sorted_and_unique() {
        assert!(TOOLS.windows(2).all(|w| w[0] < w[1]), "TOOLS sorted, no name twice");
        assert!(TOOLS.contains(&BLAZE), "blaze is a registered tool");
        assert!(KNOWN.windows(2).all(|w| w[0].name < w[1].name), "KNOWN sorted, no name twice");
        // `args` is the one a stage checks (the body check's `args-inline`, rule 83).
        assert!(
            KNOWN.iter().all(|k| k.state == State::Unchecked || (k.name == "args" && k.state == State::Checked(crate::tool_attrs::Stage::Syntax))),
            "no stage checks a blaze attribute in this build yet but `args`"
        );
    }

    #[test]
    fn known_names_classify_as_known() {
        for (i, k) in KNOWN.iter().enumerate() {
            assert_eq!(classify(k.name), BlazeAttr::Known(i as u16), "{}", k.name);
        }
    }

    #[test]
    fn unknown_names_get_the_nearest_within_two() {
        let name = |r: BlazeAttr| match r {
            BlazeAttr::Unknown { nearest: Some(i) } => Some(KNOWN[usize::from(i)].name),
            BlazeAttr::Unknown { nearest: None } => None,
            BlazeAttr::Known(i) => panic!("`{}` is known", KNOWN[usize::from(i)].name),
        };
        assert_eq!(name(classify("noaloc")), Some("noalloc"), "one deletion");
        assert_eq!(name(classify("nopanick")), Some("nopanic"), "one insertion");
        assert_eq!(name(classify("lineer")), Some("linear"), "one substitution");
        assert_eq!(name(classify("seald_")), Some("sealed"), "two edits");
        assert_eq!(name(classify("whatever")), None, "nothing within two");
        assert_eq!(name(classify("")), None, "the empty name is near no known name");
        assert_eq!(name(classify("noalloc::x")), None, "a longer path is no known name");
    }
}

/// Refinement: a key index page answers each key with its entry, in any order given.
#[test]
fn a_key_index_answers_each_key_in_one_probe() {
    blaze_sim::check(Config::cases(300), |tape| {
        let mut entries = Vec::new_in(blaze_own::testing::heap().alloc());
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
            let mut out = KeyIndex::new_in(key(255), blaze_own::testing::heap().alloc());
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
fn surface(rows: &[ScopeEntry<&str>]) -> MemSurface<'static> {
    let mut surface = MemSurface::new_in(KRATE, blaze_own::testing::heap().alloc());
    let items = FileItems {
        file: file(0),
        text: "fn a() {}",
        items: &[file_items::F],
        attrs: &[],
        file_attrs: crate::file_items::AttrSpan { start: 0, end: 0 },
        errors: &[],
        messages: &[],
        file_flags: crate::file_items::ItemFlags::NONE,
    };
    surface.insert(Fact::FileItems, &items.write(blaze_own::testing::heap().alloc()).expect("small")).expect("a written record");
    surface.insert_text(file(0), "fn a() {}");
    let mut page = KeyIndex::new_in(key(200), blaze_own::testing::heap().alloc());
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

/// Refinement: through `Items`, `lookup` finds exactly what `scope` lists.
#[test]
fn lookup_finds_what_scope_lists() {
    blaze_sim::check(Config::cases(200), |tape| {
        let rows = bindings(tape);
        let surface = surface(&rows);
        let module = key(200);
        let scope = surface.scope(module).map_err(|error| Broke::Oracle(error.to_string()))?;
        for entry in scope.bindings() {
            let found = surface.lookup(module, entry.ns, entry.name);
            if found != Ok(Some(entry.binding)) {
                return Err(Broke::Oracle(format!("lookup of {entry:?} gave {found:?}")));
            }
        }
        for ns in SPACES {
            for name in NAMES {
                let listed = scope.get(ns, name);
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
    let binding = Binding {
        def: Def { key: key(1), kind: DefKind::Other },
        vis: Visibility::Public,
        origin: Origin::Declared,
        expansion: 0,
        state: BindingState::Determined,
    };
    let rows = [ScopeEntry { ns: Namespace::Values, name: "a", binding }];
    let surface = surface(&rows);
    let mut locals: LocalTable<'static, AnyItem> = LocalTable::new_in(blaze_own::testing::heap().alloc());
    let module = locals.intern(key(200)).expect("room");
    assert_eq!(locals.intern(key(200)), Ok(module));
    let found = surface.local_lookup(&mut locals, module, Namespace::Values, "a").expect("bound");
    let Some((item, found)) = found else { panic!("`a` is bound") };
    assert_eq!(found, binding);
    assert_eq!(locals.key(item), Ok(key(1)));
    assert_eq!(surface.local_kind(&locals, item), Ok(ItemKind::Function));
    assert_eq!(surface.local_parent(&mut locals, item), Ok(Some(module)));
    let mut other: LocalTable<'static, AnyItem> = LocalTable::new_in(blaze_own::testing::heap().alloc());
    let foreign = other.intern(key(1)).and_then(|_| other.intern(key(2))).expect("room");
    assert_eq!(locals.len(), 2);
    assert_eq!(surface.local_kind(&locals, foreign), Ok(ItemKind::Function));
    let mut third: LocalTable<'static, AnyItem> = LocalTable::new_in(blaze_own::testing::heap().alloc());
    let far = (0..5).map(|n| third.intern(key(n + 10))).last().expect("five").expect("room");
    assert_eq!(locals.key(far), Err(SurfaceError::NoLocal { local: 4 }));
}

#[test]
fn a_textual_entry_answers_the_last_definition_of_a_name() {
    let mut out = TextualEntry::new_in(key(200), blaze_own::testing::heap().alloc());
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
    let mut out = ModuleFile::new_in(key(200), file(1), "src/a.rs", None, blaze_own::testing::heap().alloc()).expect("small");
    out.absent("src/a/mod.rs").expect("small");
    out.absent("src/a.rs.in").expect("small");
    out.absent("src/a/mod.rs").expect("small");
    let buffer = Aligned::copy_from(&out.finish().expect("small"));
    let view = ModuleFileView::read(&buffer).expect("a written record");
    assert_eq!((view.module(), view.file(), view.path(), view.inline()), (key(200), file(1), "src/a.rs", None));
    assert!(view.absent().eq(["src/a.rs.in", "src/a/mod.rs"]));
}

#[test]
fn a_macro_def_reads_back_with_its_helpers_in_order() {
    let krate = CrateItemKey(ContentId([7; 32]));
    let head = MacroDefHead {
        item: key(5),
        name: "Serialize",
        content: ContentId([6; 32]),
        place: Some(Place::File { file: file(2), index: ItemIndex(3) }),
        kind: MacroKind::ProcMacro { krate, kind: ProcKind::Derive },
        site: DefSite { krate, module: None },
        edition: Edition::E2021,
        exported: false,
        local_inner: false,
        written_transparency: None,
        transparency: Transparency::Opaque,
        body: None,
        params: None,
    };
    let mut out = MacroDef::new_in(head, blaze_own::testing::heap().alloc());
    out.helper("serde").expect("small");
    out.helper("allow").expect("small");
    let buffer = Aligned::copy_from(&out.finish().expect("small"));
    let view = MacroDefView::read(&buffer).expect("a written record");
    assert_eq!(view.head(), head);
    assert!(view.helpers().eq(["serde", "allow"]));
    let builtin = MacroDefHead {
        place: None,
        kind: MacroKind::BuiltinDerive(BuiltinDerive::Clone),
        ..head
    };
    let buffer = Aligned::copy_from(&MacroDef::new_in(builtin, blaze_own::testing::heap().alloc()).finish().expect("small"));
    assert_eq!(MacroDefView::read(&buffer).expect("a written record").head(), builtin);
}

