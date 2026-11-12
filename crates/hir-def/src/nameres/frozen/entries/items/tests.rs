//! Tests of per-item entries by counts (AGENTS.md rule 65): edit one item of N and N-1 entries
//! are found with none of the saved work run; each with its control.

use base_db::{Db, EditionedFileId};
use hir_expand::fixture;

use super::{
    ASSOC, ItemContainerId, KEYED_BY_FILE, LAYOUT, RAN, decode_assoc, decode_flags,
};
use crate::{
    AssocItemId, AttrDefId, ImplId,
    attrs::AttrFlags,
    nameres::{
        assoc::ImplItems,
        crate_def_map,
        frozen::{
            entries::{Counts, Memory, set},
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
    let container = ItemContainerId::ImplId(impls(&db)[0]);
    // alloc: a test's bytes
    let mut bytes = (LAYOUT + 1).to_le_bytes().to_vec();
    bytes.extend_from_slice(&0u32.to_le_bytes());
    let why = decode_assoc(&db, file, container, &bytes).expect_err("another layout");
    // alloc: a test's text
    assert!(why.contains(&format!("layout {}", LAYOUT + 1)), "{ASSOC}: {why}");
    // The control: this layout's empty value reads.
    bytes[..4].copy_from_slice(&LAYOUT.to_le_bytes());
    assert_eq!(decode_assoc(&db, file, container, &bytes).map(|it| it.len()), Ok(0));
    assert!(decode_flags(&[0; 15]).is_err());
}
