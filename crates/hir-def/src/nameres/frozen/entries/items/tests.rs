//! Tests of per-item entries by counts (AGENTS.md rule 65): edit one item of N and N-1 entries
//! are found with none of the saved work run; each with its control.

use base_db::{Db, EditionedFileId};
use hir_expand::{Lookup, declarative::stored, fixture};

use super::{
    ASSOC, IDS_IN_VALUE, ItemContainerId, KEYED_BY_FILE, LAYOUT, RAN, decode_assoc, decode_flags,
};
use crate::{
    AssocItemId, AttrDefId, ImplId,
    attrs::AttrFlags,
    nameres::{
        assoc::ImplItems,
        crate_def_map,
        frozen::{
            Unreadable,
            entries::{Counts, Memory, differ, set},
            tests::crate_named,
        },
    },
};

const IMPLS: usize = 4;

/// A crate of [`IMPLS`] plain impls, the third's `g` taking `third_sig` and its `f` answering
/// `third_body`, and a dependent.
fn text(third_sig: &str, third_body: &str) -> String {
    let mut text = String::from("//- /main.rs crate:main deps:lib\nfn main() {}\n//- /lib.rs crate:lib\n");
    for at in 0..IMPLS {
        let (sig, body) = if at == 2 { (third_sig, third_body) } else { ("&self", "0") };
        // alloc: a test's fixture
        text.push_str(&format!(
            "pub struct S{at};\nimpl S{at} {{\n    pub fn f(&self) -> u32 {{ {body} }}\n    \
             #[inline]\n    pub fn g({sig}) {{}}\n    pub const C{at}: u8 = 1;\n}}\n"
        ));
    }
    text
}

fn impls(db: &Db) -> Vec<ImplId> {
    let lib = crate_named(db, "lib");
    let def_map = crate_def_map(db, lib);
    // alloc: a test's list
    let impls: Vec<_> = def_map.module(db, def_map.root).scope(db).impls().collect();
    assert_eq!(impls.len(), IMPLS);
    impls
}

/// The assoc items of every impl, as text no database's ids are in.
fn ask_assoc(db: &Db) -> Vec<String> {
    impls(db)
        .into_iter()
        .map(|it| {
            let (items, diagnostics) = ImplItems::of(db, it);
            assert_eq!(diagnostics.iter().count(), 0);
            // alloc: a test's text
            let names: Vec<_> = items
                .items
                .iter()
                .map(|(name, item)| {
                    let kind = match item {
                        AssocItemId::FunctionId(_) => "fn",
                        AssocItemId::ConstId(_) => "const",
                        AssocItemId::TypeAliasId(_) => "type",
                    };
                    format!("{kind} {}", name.as_str())
                })
                .collect();
            names.join(", ")
        })
        // alloc: a test's list
        .collect()
}

/// The flags of every impl and of every assoc `fn`.
fn ask_flags(db: &Db) -> Vec<u128> {
    // alloc: a test's list
    let mut all = Vec::new();
    for it in impls(db) {
        all.push(AttrFlags::query(db, AttrDefId::ImplId(it)).bits());
        for (_, item) in ImplItems::of(db, it).0.items.iter() {
            if let AssocItemId::FunctionId(function) = *item {
                all.push(AttrFlags::query(db, AttrDefId::FunctionId(function)).bits());
            }
        }
    }
    all
}

fn ran<T>(work: impl FnOnce() -> T) -> (T, u64) {
    RAN.set(0);
    let out = work();
    (out, RAN.get())
}

/// What a database with `store` asks of it for `text`'s assoc items, and how many collections
/// ran.
fn assoc_counts(text: &str, store: &std::sync::Arc<Memory>) -> (Vec<String>, Counts, u64) {
    let mut db = fixture::db(text).into_db();
    // clone: a count of the test's store
    set(&mut db, store.clone());
    _ = store.take();
    let (got, ran) = ran(|| ask_assoc(&db));
    (got, store.take(), ran)
}

/// Rule 65 by counts: the signature of one method of one impl of N edited. The file's index
/// is made again (its content is new); the assoc items of the N-1 other impls are found
/// under their own keys and their collector does not run; the edited impl's are collected.
#[test]
fn one_impl_of_n_edited_finds_the_assoc_items_of_the_others() {
    let n = IMPLS as u64;
    let store = Memory::new();
    let (_, made, collected) = assoc_counts(&text("&self", "0"), &store);
    assert_eq!((made.made, made.hits, collected), (1 + n, 0, n), "{made:?}");

    let after = text("&self, by: u8", "0");
    let want = ask_assoc(&fixture::db(&after).into_db());
    let (got, counts, collected) = assoc_counts(&after, &store);
    assert_eq!(got, want);
    assert_eq!(counts, Counts { hits: n - 1, misses: 2, made: 2, again: 0 });
    assert_eq!(collected, 1, "only the edited impl is collected");
}

/// The control of the above: the planted bug. Keys that also take the file's content, as a
/// key of a file or a crate does, miss all N after the same edit and collect all N.
#[test]
fn keyed_by_the_file_the_same_edit_misses_every_impl() {
    let n = IMPLS as u64;
    KEYED_BY_FILE.set(true);
    let store = Memory::new();
    _ = assoc_counts(&text("&self", "0"), &store);
    let (_, counts, collected) = assoc_counts(&text("&self, by: u8", "0"), &store);
    KEYED_BY_FILE.set(false);
    assert_eq!(counts, Counts { hits: 0, misses: 1 + n, made: 1 + n, again: 0 });
    assert_eq!(collected, n);
}

/// A body's edit makes no flags entry and no assoc entry again: only the file's index. (The
/// control is the signature's edit above, which does.)
#[test]
fn a_body_edit_makes_no_flags_or_assoc_entry_again() {
    let store = Memory::new();
    let mut writing = fixture::db(&text("&self", "0")).into_db();
    // clone: a count of the test's store
    set(&mut writing, store.clone());
    _ = (ask_assoc(&writing), ask_flags(&writing));
    _ = store.take();

    let after = text("&self", "40 + 2");
    let from_source = fixture::db(&after).into_db();
    let want = (ask_assoc(&from_source), ask_flags(&from_source));
    let mut edited = fixture::db(&after).into_db();
    // clone: a count of the test's store
    set(&mut edited, store.clone());
    let (got, work) = ran(|| (ask_assoc(&edited), ask_flags(&edited)));
    assert_eq!(got, want);
    let counts = store.take();
    assert_eq!((counts.misses, counts.made, counts.again, work), (1, 1, 0, 0), "{counts:?}");
}

/// Two databases of one text keep the same bytes under the same keys.
#[test]
fn two_databases_keep_identical_entries() {
    let text = text("&self", "0");
    let stores = [Memory::new(), Memory::new()];
    for store in &stores {
        let mut db = fixture::db(&text).into_db();
        // clone: a count of the test's store
        set(&mut db, store.clone());
        _ = (ask_assoc(&db), ask_flags(&db));
    }
    let (first, second) = (stores[0].held.lock(), stores[1].held.lock());
    assert_eq!(first.len(), second.len());
    assert!(first.len() > IMPLS);
    for (key, bytes) in first.iter() {
        assert_eq!(second.get(key), Some(bytes), "{key:?}");
    }
}

#[test]
fn a_previous_layout_of_assoc_items_is_refused_by_name() {
    let db = fixture::db(&text("&self", "0")).into_db();
    let lib = crate_named(&db, "lib");
    let data = lib.data(&db);
    let file =
        EditionedFileId::new_with_extensions(&db, data.root_file_id, data.edition, lib.extensions(&db));
    let first = impls(&db)[0];
    let container = ItemContainerId::ImplId(first);
    // The three assoc items written in `impl S0`: `f`, `g` and `C0`.
    let listed = stored::index::of(&db, file, lib).assoc_items(first.lookup(&db).id.value.erase());
    assert_eq!(listed.len(), 3);
    let file = hir_expand::HirFileId::FileId(file);
    // alloc: a test's bytes
    let mut bytes = (LAYOUT + 1).to_le_bytes().to_vec();
    bytes.extend_from_slice(&0u32.to_le_bytes());
    let why = decode_assoc(&db, file, listed, container, &bytes).expect_err("another layout");
    // alloc: a test's text
    assert!(why.contains(&format!("layout {}", LAYOUT + 1)), "{ASSOC}: {why}");
    // The control: this layout's empty value reads.
    bytes[..4].copy_from_slice(&LAYOUT.to_le_bytes());
    assert_eq!(decode_assoc(&db, file, listed, container, &bytes).map(|it| it.len()), Ok(0));
    assert!(decode_flags(&[0; 15]).is_err());

    // A value of one item of `kind` (0 a `fn`) at the place `at` among those written.
    let value = |kind: u8, at: u32, name: &str| {
        // alloc: a test's bytes
        let mut bytes = LAYOUT.to_le_bytes().to_vec();
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.push(kind);
        bytes.extend_from_slice(&at.to_le_bytes());
        bytes.extend_from_slice(&(name.len() as u32).to_le_bytes());
        bytes.extend_from_slice(name.as_bytes());
        bytes
    };
    let read = |bytes: &[u8]| decode_assoc(&db, file, listed, container, bytes).map(|it| it.len());
    assert_eq!(read(&value(0, 0, "f")), Ok(1));
    // A value of another `impl` (bytes under a key that does not cover them) is refused by
    // what it names that this one does not have: a place past the last written, and a `fn`
    // where a `const` is written.
    let why = read(&value(0, 3, "h")).expect_err("a place past the last");
    assert!(why.contains("names assoc item 3 (`h`)") && why.contains("lists 3"), "{why}");
    let why = read(&value(0, 2, "C0")).expect_err("a fn where a const is written");
    assert!(why.contains("as kind 0"), "{why}");
}

/// Two modules of one file, each with a struct `S` and an `impl S` written the same but for
/// the bodies, which no header covers: one key for both, and each impl's items at their own
/// ast ids (an impl's id counts the file's impls of its self type and trait before it, and an
/// assoc item's id is of its impl's). `once_cell`'s `unsync` and `sync` modules are this.
const TWINS: &str = "//- /main.rs crate:main deps:lib\nfn main() {}\n//- /lib.rs crate:lib\n\
pub mod a {\n    pub struct S;\n    impl S {\n        pub fn f(&self) -> u32 { 1 }\n        \
pub const C: u8 = 1;\n    }\n}\n\
pub mod b {\n    pub struct S;\n    impl S {\n        pub fn f(&self) -> u32 { 2 }\n        \
pub const C: u8 = 2;\n    }\n}\n";

/// Every impl of the lib, of every module, by its ast id: the same order in every database
/// of one text.
fn all_impls(db: &Db) -> Vec<ImplId> {
    let def_map = crate_def_map(db, crate_named(db, "lib"));
    // alloc: a test's list
    let mut impls: Vec<ImplId> =
        def_map.modules(db).flat_map(|(_, module)| module.scope(db).impls()).collect();
    impls.sort_by_key(|it| it.lookup(db).id.value.erase().into_raw());
    impls
}

/// The assoc items of every impl of the lib, each with the ast id it has: what tells the
/// items of one impl from those of another written the same. An ast id is made of the file's
/// text, so it is the same in every database of one text.
fn ask_placed(db: &Db) -> Vec<String> {
    all_impls(db)
        .into_iter()
        .map(|it| {
            let (items, diagnostics) = ImplItems::of(db, it);
            assert_eq!(diagnostics.iter().count(), 0);
            // alloc: a test's text
            let placed: Vec<_> = items
                .items
                .iter()
                .map(|(name, item)| {
                    let id = match *item {
                        AssocItemId::FunctionId(it) => it.lookup(db).id.value.erase(),
                        AssocItemId::ConstId(it) => it.lookup(db).id.value.erase(),
                        AssocItemId::TypeAliasId(it) => it.lookup(db).id.value.erase(),
                    };
                    format!("{} {id:?}", name.as_str())
                })
                .collect();
            placed.join(", ")
        })
        // alloc: a test's list
        .collect()
}

/// The invariant: two impls written the same have one key and one value, and each reads its
/// own assoc items from it: the value names an item by its place among those written, and
/// the reader takes the ast id its own file's index has at that place. What the build of
/// 2026-11-12 was refused for, `std` and `once_cell` (`agents/annoyances/assoc-key-1.json`).
/// Uses nothing the head before the fix lacks, so pasted there it fails as that head did.
#[test]
fn two_impls_written_the_same_each_read_their_own_assoc_items() {
    let want = ask_placed(&fixture::db(TWINS).into_db());
    assert_eq!(want.len(), 2);
    assert_ne!(want[0], want[1], "the control: the same names at other ast ids");

    let store = Memory::new();
    let mut writing = fixture::db(TWINS).into_db();
    // clone: a count of the test's store
    set(&mut writing, store.clone());
    let (got, collected) = ran(|| ask_placed(&writing));
    assert_eq!(got, want, "the second impl found the first's entry and read its own items");
    let made = store.take();
    // The file's index and one assoc entry made; the second impl's is a hit.
    assert_eq!((made.made, made.hits, made.again, collected), (2, 1, 0, 1), "{made:?}");

    let mut reading = fixture::db(TWINS).into_db();
    // clone: a count of the test's store
    set(&mut reading, store.clone());
    let (got, collected) = ran(|| ask_placed(&reading));
    assert_eq!(got, want);
    assert_eq!(collected, 0, "both are found");
}

/// The same within one build, as the driver holds entries (`Memory::within_one_build`):
/// neither impl finds the other's entry, each makes its value and puts it, and the second
/// put is the bytes of the first.
#[test]
fn two_impls_written_the_same_make_one_value_within_one_build() {
    let want = ask_placed(&fixture::db(TWINS).into_db());
    let store = Memory::within_one_build();
    let mut db = fixture::db(TWINS).into_db();
    // clone: a count of the test's store
    set(&mut db, store.clone());
    let (got, collected) = ran(|| ask_placed(&db));
    assert_eq!(got, want);
    let counts = store.take();
    assert_eq!((counts.made, counts.again, counts.hits, collected), (2, 1, 0, 2), "{counts:?}");
}

/// The control of both, and of the invariants of this file (`scripts/src/bin/invariants.rs`
/// reads its name): the planted bug, layout 1's value, an ast id where the place is. A
/// store that serves a put back hands the second impl the first's items (a function with
/// another function's body); within one build the second put is two values under one key,
/// and the refusal names the impl, its file and where the two values part.
#[test]
fn the_oracle_catches_an_ast_id_in_an_assoc_value() {
    let want = ask_placed(&fixture::db(TWINS).into_db());
    IDS_IN_VALUE.set(true);
    let store = Memory::new();
    let mut served = fixture::db(TWINS).into_db();
    // clone: a count of the test's store
    set(&mut served, store.clone());
    let got = ask_placed(&served);

    let within = Memory::within_one_build();
    let mut building = fixture::db(TWINS).into_db();
    // clone: a count of the test's store
    set(&mut building, within.clone());
    let refused =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ask_placed(&building)));
    IDS_IN_VALUE.set(false);

    // clone: a test's text
    assert_eq!(got, [want[0].clone(), want[0].clone()], "the second impl read the first's ids");
    let payload = refused.expect_err("two values under one key");
    let why = Unreadable::of_payload(&*payload).expect("an `Unreadable`").to_string();
    for part in
        ["assoc items", "`impl S {`", "lib.rs:", "two values under key", "they first differ at byte"]
    {
        assert!(why.contains(part), "no {part:?} in: {why}");
    }
}

/// Asks the assoc items and the flags of every impl of the lib, and the flags of each assoc
/// `fn`.
fn ask_all(db: &Db) {
    for it in all_impls(db) {
        _ = AttrFlags::query(db, AttrDefId::ImplId(it));
        for (_, item) in ImplItems::of(db, it).0.items.iter() {
            if let AssocItemId::FunctionId(function) = *item {
                _ = AttrFlags::query(db, AttrDefId::FunctionId(function));
            }
        }
    }
}

/// The invariant, over every family asked here: equal keys are equal bytes, whichever file
/// an item is in and wherever in it. The second file is the first with a copy of every item
/// in a module written before them, so every impl has a twin and every ast id of the first
/// file's impls and their items is another. Each store holds a file as one build does (a put
/// of other bytes under a key it holds is refused), and every entry of the first file but
/// its index is found, byte for byte, among the second's.
#[test]
fn an_entry_is_the_same_bytes_wherever_its_item_is_written() {
    let one = text("&self", "0");
    let (head, items) = one.split_once("crate:lib\n").expect("the lib's file");
    // alloc: a test's fixture
    let two = format!("{head}crate:lib\npub mod twin {{\n{items}}}\n{items}");
    let stores = [Memory::within_one_build(), Memory::within_one_build()];
    for (store, text) in stores.iter().zip([&one, &two]) {
        let mut db = fixture::db(text).into_db();
        // clone: a count of the test's store
        set(&mut db, store.clone());
        ask_all(&db);
    }
    let again = stores[1].take().again;
    assert!(again >= IMPLS as u64, "each twin put the bytes of its twin: {again}");
    let (first, second) = (stores[0].held.lock(), stores[1].held.lock());
    let mut shared = 0;
    for (key, bytes) in first.iter() {
        if let Some(other) = second.get(key) {
            assert_eq!(other, bytes, "{key:?}");
            shared += 1;
        }
    }
    assert_eq!(shared, first.len() - 1, "every entry of the first file but its index");
    // An assoc entry an impl; the flags of items with the same attributes are one entry.
    assert!(shared > IMPLS, "{shared} entries shared");
}

/// A refusal of two values says how they differ: the lengths, the first byte that differs
/// and the bytes there.
#[test]
fn two_values_are_told_apart_by_length_and_first_byte() {
    let said = differ(b"abcd", b"abXd");
    assert!(said.contains("is 4 bytes and the one given 4"), "{said}");
    assert!(said.contains("first differ at byte 2"), "{said}");
    assert!(said.contains("has 6364 and the given one 5864"), "{said}");
    let said = differ(b"ab", b"abc");
    assert!(said.contains("is 2 bytes and the one given 3"), "{said}");
    assert!(said.contains("first differ at byte 2"), "{said}");
    assert!(said.contains("has its end and the given one 63"), "{said}");
}
