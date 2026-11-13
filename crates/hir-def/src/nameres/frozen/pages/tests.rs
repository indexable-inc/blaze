//! Tests of a crate's `DefMap` read from pages: each with its control, a planted bug or the
//! cause removed.

use std::sync::Arc;

use base_db::{Crate, Db};
use blaze_core::lock::Lock;
use blaze_hash::HashSet;
use blaze_ids::ContentId;
use hir_expand::{fixture, name::Name};
use intern::Symbol;
use std::alloc::Global;

use super::{
    Enc, HEADER, LAYOUT, PageSource, Paged, encode, encode_maps, names_of, read_head, set,
};
use crate::{
    nameres::{
        crate_def_map, crate_local_def_map,
        frozen::tests::{FIXTURE, crate_named},
    },
    per_ns::PerNs,
};

/// A page of the store's pool.
const PAGE: u64 = 4096;

/// A record in memory that notes each page a read touches.
struct Counting {
    bytes: Vec<u8>,
    touched: Lock<HashSet<u64, Global>>,
}

impl Counting {
    fn new(bytes: Vec<u8>) -> Arc<Counting> {
        let touched = Lock::new("test.def.pages.touched", HashSet::new_in(Global));
        Arc::new(Counting { bytes, touched })
    }

    /// The pages read since the last call.
    fn take(&self) -> usize {
        let mut touched = self.touched.lock();
        let pages = touched.len();
        touched.clear();
        pages
    }
}

impl PageSource for Counting {
    fn size(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn read(&self, at: u64, len: u64, with: &mut dyn FnMut(&[u8])) -> Result<(), String> {
        let part = self
            .bytes
            .get(at as usize..(at + len) as usize)
            .ok_or_else(|| format!("bytes {at}+{len} of {}", self.bytes.len()))?;
        if len != 0 {
            self.touched.lock().extend(at / PAGE..=(at + len - 1) / PAGE);
        }
        with(part);
        Ok(())
    }
}

/// A database of `text` that reads each crate of `pages` from its record.
fn paged(text: &str, pages: &[(&str, Arc<Counting>)]) -> Db {
    let mut db = fixture::db(text).into_db();
    let maps = pages
        .iter()
        .enumerate()
        .map(|(at, (name, source))| {
            let key = ContentId([at as u8 + 1; 32]);
            (crate_named(&db, name), Paged { key, source: source.clone() })
        })
        .collect();
    set(&mut db, maps);
    db
}

/// The pages of each crate of `names`, written by a fresh database of `text`.
fn written(text: &str, names: &[&'static str]) -> Vec<(&'static str, Vec<u8>)> {
    let db = fixture::db(text).into_db();
    names.iter().map(|&name| (name, encode(&db, crate_named(&db, name)).unwrap())).collect()
}

fn sources(pages: &[(&'static str, Vec<u8>)]) -> Vec<(&'static str, Arc<Counting>)> {
    // clone: a test's copy of a record
    pages.iter().map(|(name, bytes)| (*name, Counting::new(bytes.clone()))).collect()
}

fn plain(text: &str) -> Name {
    Name::new_symbol_root(Symbol::intern(text))
}

/// What `found` is, with nothing of one database in it: two databases' answers compare by it.
fn print(db: &Db, krate: Crate, found: PerNs) -> Vec<u8> {
    let mut enc = Enc::new(db, krate);
    enc.fingerprint(|enc| enc.per_ns(found)).unwrap()
}

/// The refinement: a database that reads each crate of `names` from its pages answers as
/// `resolved`, which resolved it from source. Compared two ways: the crate's whole `DefMap`
/// written as [`super::super::encode`] writes it (every scope, the module tree, the records,
/// the preludes: each accessor the encoder reads), and each name of each module looked up
/// alone, with one name no module has.
fn agree(resolved: &Db, read: &Db, names: &[&str]) -> Result<(), String> {
    for &name in names {
        let (a, b) = (crate_named(resolved, name), crate_named(read, name));
        let whole = super::super::encode(resolved, a)?;
        if super::super::encode(read, b)? != whole {
            return Err(format!("{name}: read from pages, its def map is written otherwise"));
        }
        let (map_a, map_b) = (crate_def_map(resolved, a), crate_def_map(read, b));
        let (table_a, table_b) = (map_a.table(resolved), map_b.table(read));
        if table_a.len() != table_b.len() {
            return Err(format!("{name}: {} modules, {} read", table_a.len(), table_b.len()));
        }
        let mut looked = 0;
        for (index, ((id_a, data), (id_b, data_b))) in table_a.iter().zip(table_b.iter()).enumerate() {
            // The traits in scope, from their segment.
            let traits = |db: &Db, krate: Crate, it: &crate::nameres::ModuleData| {
                let all: Vec<_> = it.traits_in_scope(db).collect();
                let mut enc = Enc::new(db, krate);
                enc.fingerprint(|enc| {
                    all.iter().try_for_each(|&it| enc.def(crate::ModuleDefId::TraitId(it)))
                })
                .unwrap()
            };
            if traits(resolved, a, data) != traits(read, b, data_b) {
                return Err(format!("{name}: the traits in scope in module {index} differ"));
            }
            // Each textual macro, from its table, and one no module has.
            let mut textual: Vec<Name> =
                data.scope(resolved).legacy_macros().map(|(it, _)| *it).collect();
            textual.retain(|it| it.key().is_none());
            textual.push(plain("no_textual_macro_has_this"));
            for it in textual {
                let ids = |db: &Db, krate: Crate, data: &crate::nameres::ModuleData| {
                    let found = data.legacy_macro_in_scope(db, &it).map(<[_]>::to_vec);
                    let mut enc = Enc::new(db, krate);
                    enc.fingerprint(|enc| {
                        enc.option(found, |enc, ids| ids.into_iter().try_for_each(|id| enc.macro_id(id)))
                    })
                    .unwrap()
                };
                if ids(resolved, a, data) != ids(read, b, data_b) {
                    return Err(format!(
                        "{name}: textual macro `{}` in module {index} differs",
                        it.as_str()
                    ));
                }
            }
            // The items the impls maintainer is handed of a served crate, against its scope.
            if crate::nameres::frozen::pages::is_paged(read, b) {
                let got = super::module_items(read, b, index);
                let scope = data.scope(resolved);
                let print = |db: &Db, krate: Crate, impls: &[crate::ImplId], traits: &[crate::TraitId], derives: usize| {
                    let mut enc = Enc::new(db, krate);
                    enc.fingerprint(|enc| {
                        impls.iter().try_for_each(|&it| enc.impl_id(it))?;
                        enc.usize(derives)?;
                        traits.iter().try_for_each(|&it| enc.def(crate::ModuleDefId::TraitId(it)))
                    })
                    .unwrap()
                };
                let impls: Vec<_> = scope.impls().collect();
                let declared: Vec<crate::TraitId> = scope
                    .declarations()
                    .filter_map(|it| match it {
                        crate::ModuleDefId::TraitId(it) => Some(it),
                        _ => None,
                    })
                    .collect();
                let was = print(resolved, a, &impls, &declared, scope.builtin_derive_impls().len());
                let is = print(read, b, &got.impls, &got.traits, got.derive_impls.len());
                if was != is {
                    return Err(format!("{name}: the items of module {index} differ"));
                }
            }
            let mut asked = names_of(data.scope(resolved));
            // A keyed name holds a context of the database that made it.
            asked.retain(|it| it.key().is_none());
            asked.push(plain("a_name_no_module_has"));
            for it in asked {
                let was = print(resolved, a, map_a.name_in_module(resolved, id_a, &it));
                let is = print(read, b, map_b.name_in_module(read, id_b, &it));
                if was != is {
                    return Err(format!(
                        "{name}: `{}` in module {index} means something else read from pages",
                        it.as_str()
                    ));
                }
                looked += 1;
            }
        }
        if looked <= table_a.len() {
            return Err(format!("{name}: no name was looked up"));
        }
    }
    Ok(())
}

/// The refinement: a crate read from pages answers every def map query as the crate resolved
/// from source does, whether its dependency is read from pages too or resolved.
#[test]
fn a_crate_read_from_pages_answers_as_the_one_resolved() {
    let resolved = fixture::db(FIXTURE).into_db();
    let pages = written(FIXTURE, &["lib", "main"]);
    let both = paged(FIXTURE, &sources(&pages));
    assert_eq!(agree(&resolved, &both, &["lib", "main"]), Ok(()));
    let lib_alone = paged(FIXTURE, &sources(&pages[..1]));
    assert_eq!(agree(&resolved, &lib_alone, &["lib", "main"]), Ok(()));
}

/// The control: the same oracle over pages whose name tables leave one name out.
#[test]
fn the_oracle_catches_a_name_left_out_of_its_table() {
    let resolved = fixture::db(FIXTURE).into_db();
    let lib = crate_named(&resolved, "lib");
    let pair = crate_local_def_map(&resolved, lib);
    let planted =
        encode_maps(&resolved, lib, pair.def_map(&resolved), pair.local(&resolved), Some("Shown"))
            .unwrap();
    let read = paged(FIXTURE, &[("lib", Counting::new(planted))]);
    let said = agree(&resolved, &read, &["lib"]).unwrap_err();
    assert!(said.contains("`Shown`"), "{said}");
}

/// Two databases that asked for things in another order, and so numbered their ids
/// otherwise, write the same bytes.
#[test]
fn the_same_pages_from_two_databases() {
    let first = written(FIXTURE, &["lib", "main"]);
    let other = fixture::db(FIXTURE).into_db();
    let (main, lib) = (crate_named(&other, "main"), crate_named(&other, "lib"));
    // The dependent first, its whole def map written before anything of `lib` is asked for.
    _ = super::super::encode(&other, main).unwrap();
    let second = [("lib", encode(&other, lib).unwrap()), ("main", encode(&other, main).unwrap())];
    assert_eq!(first.as_slice(), second.as_slice());
    // The comparison is of something: the two crates' records differ, and are records.
    assert_ne!(first[0].1, first[1].1);
    assert!(first.iter().all(|(_, bytes)| bytes.len() > HEADER));
}

/// The control of the comparison above: one name changed in the source is other bytes.
#[test]
fn the_pages_of_another_source_are_other_bytes() {
    let changed = FIXTURE.replace("pub struct Shown;", "pub struct Shewn;");
    assert_ne!(changed, FIXTURE);
    let first = written(FIXTURE, &["lib"]);
    let second = written(&changed, &["lib"]);
    assert_ne!(first, second);
}

/// Why `bytes`, given as the pages of `lib`, do not read; `None` when they do.
fn refused(bytes: Vec<u8>) -> Option<String> {
    let source = Counting::new(bytes);
    // clone: a count of the test's source
    let db = paged(FIXTURE, &[("lib", source.clone())]);
    read_head(&db, crate_named(&db, "lib"), &*source).err()
}

/// A record of a previous layout, of another encoding or of another kind is refused by name,
/// and so is one cut short; the record as written reads (the control).
#[test]
fn a_previous_or_foreign_layout_is_refused_by_name() {
    let pages = written(FIXTURE, &["lib"]).remove(0).1;
    // clone: a test's copies of a record
    assert_eq!(refused(pages.clone()), None);

    let mut previous = pages.clone();
    previous[8..12].copy_from_slice(&(LAYOUT + 1).to_le_bytes());
    let said = refused(previous).unwrap();
    assert!(said.contains(&format!("layout {}", LAYOUT + 1)), "{said}");

    // Layout 2, whose names were written twice and whose rows had no runs: refused by name,
    // never read with this layout's rows.
    assert_eq!(LAYOUT, 3);
    let mut twice = pages.clone();
    twice[8..12].copy_from_slice(&2u32.to_le_bytes());
    let said = refused(twice).unwrap();
    assert!(said.contains("def map pages of layout 2"), "{said}");

    let mut encoding = pages.clone();
    encoding[16] ^= 1;
    let said = refused(encoding).unwrap();
    assert!(said.contains("encoding"), "{said}");

    let db = fixture::db(FIXTURE).into_db();
    let foreign = super::super::encode(&db, crate_named(&db, "lib")).unwrap();
    assert!(foreign.len() > HEADER);
    let said = refused(foreign).unwrap();
    assert!(said.contains("no def map pages"), "{said}");

    let mut cut = pages;
    cut.pop();
    let said = refused(cut).unwrap();
    assert!(said.contains("header says"), "{said}");
}

/// A crate `lib` of `count` functions and as many structs, and a dependent.
fn sized(count: usize) -> String {
    let mut text = String::from("//- /main.rs crate:main deps:lib\nfn main() {}\n");
    text.push_str("//- /lib.rs crate:lib\n");
    for at in 0..count {
        text.push_str(&format!("pub fn f{at}() {{}}\npub struct S{at};\n"));
    }
    text
}

/// The most pages one name's lookup reads: a window of slots (two pages where it crosses
/// one, a third where it wraps) and the entry (two where it crosses one), and one more for a
/// name whose slot is past its window.
const LOOKUP_PAGES: usize = 6;

/// `lib` of `count` items read from pages, its head read: the record's pages, and its source.
fn sized_and_read(count: usize) -> (Db, Arc<Counting>, usize) {
    let text = sized(count);
    let pages = written(&text, &["lib"]).remove(0).1;
    let record_pages = pages.len().div_ceil(PAGE as usize);
    let source = Counting::new(pages);
    // clone: a count of the test's source
    let db = paged(&text, &[("lib", source.clone())]);
    let lib = crate_named(&db, "lib");
    _ = crate_def_map(&db, lib);
    source.take();
    (db, source, record_pages)
}

/// A lookup of one name reads the pages that name is on: the same few whether the crate is
/// 80 items or 8,000.
#[test]
fn a_lookup_reads_a_bounded_number_of_pages() {
    let mut records = Vec::new();
    for count in [40, 4000] {
        let (db, source, record_pages) = sized_and_read(count);
        let lib = crate_named(&db, "lib");
        let def_map = crate_def_map(&db, lib);
        let found = def_map.name_in_module(&db, def_map.root, &plain("f7"));
        assert!(found.values.is_some() && found.types.is_none(), "{count}: {found:?}");
        let touched = source.take();
        assert!((1..=LOOKUP_PAGES).contains(&touched), "{count} items: {touched} pages read");
        let missing = def_map.name_in_module(&db, def_map.root, &plain("no_such"));
        assert!(missing.is_none(), "{count}: {missing:?}");
        let touched = source.take();
        assert!((1..=LOOKUP_PAGES).contains(&touched), "{count} items: {touched} pages read");
        records.push(record_pages);
    }
    // The larger record is far over the bound, so the bound is the lookup's, not the record's.
    assert!(records[1] > 10 * LOOKUP_PAGES, "{records:?}");
    assert!(records[1] > 10 * records[0], "{records:?}");
}

/// The control: the cause removed. The same crate's whole scope asked for, as before a name
/// had a table, reads far more pages than the bound.
#[test]
fn a_whole_scope_reads_more_pages_than_a_name() {
    let (db, source, _) = sized_and_read(4000);
    let lib = crate_named(&db, "lib");
    let def_map = crate_def_map(&db, lib);
    let scope = def_map.module(&db, def_map.root).scope(&db);
    assert!(scope.get(&plain("f7")).values.is_some());
    let touched = source.take();
    assert!(touched > 2 * LOOKUP_PAGES, "{touched} pages read");
}

/// The oracle's own check: its `print` tells two meanings apart.
#[test]
fn the_print_of_a_meaning_tells_two_apart() {
    let db = fixture::db(FIXTURE).into_db();
    let lib = crate_named(&db, "lib");
    let def_map = crate_def_map(&db, lib);
    let shown = def_map.name_in_module(&db, def_map.root, &plain("Shown"));
    let e = def_map.name_in_module(&db, def_map.root, &plain("E"));
    assert!(!shown.is_none() && !e.is_none());
    assert_ne!(print(&db, lib, shown), print(&db, lib, e));
    assert_eq!(print(&db, lib, PerNs::none()), print(&db, lib, PerNs::none()));
}

/// What reading the pages of `lib` runs after `main`, a crate beside it, was given other
/// pages; and what reading `main`'s runs then.
fn runs_after_another_crates_gift() -> (Vec<String>, Vec<String>) {
    use crate::test_db::TestDBExt;
    let pages = written(FIXTURE, &["lib", "main"]);
    let mut db = paged(FIXTURE, &sources(&pages));
    let (lib, main) = (crate_named(&db, "lib"), crate_named(&db, "main"));
    assert!(super::paged(&db, lib).is_some() && super::paged(&db, main).is_some());
    let mut maps = super::all(&db);
    maps.get_mut(&main).unwrap().key = ContentId([9; 32]);
    set(&mut db, maps);
    let of_lib = db.executed_in_read_order(|| _ = super::paged(&db, lib));
    let of_main = db.executed_in_read_order(|| _ = super::paged(&db, main));
    assert_eq!(super::paged(&db, main).as_ref().map(|it| it.key), Some(ContentId([9; 32])));
    assert_eq!(super::paged(&db, lib).as_ref().map(|it| it.key), Some(ContentId([1; 32])));
    (of_lib, of_main)
}

/// The invariant: what a crate is read from is the crate's own slot (`deps::given`), so pages
/// given to one crate run no row of another crate; the crate given them runs its own again.
#[test]
fn pages_given_to_one_crate_run_no_row_of_another() {
    let (of_lib, of_main) = runs_after_another_crates_gift();
    assert_eq!(of_lib, Vec::<String>::new(), "`lib` was given nothing");
    assert_eq!(of_main.len(), 1, "`main`'s reader runs once: {of_main:?}");
}

/// The control: every crate given its entry again at each set, as one map of every crate
/// did to its readers, runs `lib`'s reader too.
#[test]
fn the_oracle_catches_a_gift_to_every_crate() {
    crate::deps::given::GIVES_EVERY_CRATE.set(true);
    let (of_lib, _) = runs_after_another_crates_gift();
    crate::deps::given::GIVES_EVERY_CRATE.set(false);
    assert_eq!(of_lib.len(), 1, "the planted bug writes `lib`'s slot: {of_lib:?}");
}

/// What a random edit appends to `main.rs`: items, uses of `lib`'s names (one a glob, one
/// through a module of its own) and a use of a name `lib` does not have.
const EDITS: [&str; 6] = [
    "pub fn added_N() {}\n",
    "pub use lib::E as Renamed_N;\n",
    "mod more_N { pub use lib::*; pub struct Own; }\n",
    "pub struct Holds_N(lib::Shown);\n",
    "use lib::Shown as _;\n",
    "use lib::no_such_name_N;\n",
];

/// The text of `main.rs` in [`FIXTURE`].
fn main_text() -> &'static str {
    let from = FIXTURE.find("#[macro_use]").expect("`main.rs` starts with `#[macro_use]`");
    let to = FIXTURE.find("//- /inner.rs").expect("`inner.rs` follows `main.rs`");
    &FIXTURE[from..to]
}

/// [`FIXTURE`] with `lib` in a source root of its own, as a dependency's package is: a write
/// of a file of `main` then moves no file of `lib`'s source root, which `lib`'s key covers
/// whole and whose every crate a write of one of its files withdraws
/// (`deps::serve::withdraw_moved`).
fn apart() -> String {
    let lib = "//- /lib.rs crate:lib";
    assert_eq!(FIXTURE.matches(lib).count(), 1, "`lib.rs` is declared once");
    FIXTURE.replace(lib, "//- /lib.rs crate:lib new_source_root:local")
}

/// Edits `main.rs` alike in a database of the fixture `text` that resolves `lib` from source and in
/// one that reads it from `pages`, `edits` times as `draw` picks (an index into [`EDITS`]),
/// and holds the two to [`agree`] on `lib` and `main` after each edit: the first disagreement.
fn agree_under_edits(
    text: &str,
    pages: Vec<u8>,
    edits: usize,
    mut draw: impl FnMut() -> usize,
) -> Result<(), String> {
    let mut resolved = fixture::db(text).into_db();
    let mut read = paged(text, &[("lib", Counting::new(pages))]);
    agree(&resolved, &read, &["lib", "main"])?;
    let mut text = main_text().to_owned();
    for step in 0..edits {
        text.push_str(&EDITS[draw() % EDITS.len()].replace('N', &step.to_string()));
        for db in [&mut resolved, &mut read] {
            let root = crate_named(db, "main").data(db).root_file_id;
            db.set_file_text(root, &text);
        }
        let lib = crate_named(&read, "lib");
        if super::paged(&read, lib).is_none() {
            return Err(format!("step {step}: an edit of `main` took `lib`'s pages away"));
        }
        agree(&resolved, &read, &["lib", "main"]).map_err(|why| format!("step {step}: {why}"))?;
    }
    Ok(())
}

/// The refinement: a database that reads `lib` from its pages answers every def map query
/// of `lib` and of `main`, which names it, as one that resolved `lib` from source, after
/// each of a run of random edits of `main`; and `lib`, a crate of a source root of its own,
/// stays read from its pages throughout.
#[hegel::test]
fn a_crate_read_from_pages_answers_as_the_one_resolved_under_edits_above_it(tc: hegel::TestCase) {
    use hegel::generators as gs;
    let apart = apart();
    let pages = written(&apart, &["lib"]).remove(0).1;
    let edits: usize = tc.draw(gs::integers().max_value(6));
    let said = agree_under_edits(&apart, pages, edits, || {
        tc.draw(gs::integers().max_value(EDITS.len() - 1))
    });
    assert_eq!(said, Ok(()));
}

/// The control: the cause removed. With `lib` in `main`'s source root, as [`FIXTURE`] has it,
/// the first edit of `main.rs` is a write of a file of the source root `lib`'s key covers, so
/// `lib`'s pages are withdrawn with it: the two crates agree before the edit and the run
/// stops at step 0.
#[test]
fn a_crate_of_the_edited_source_root_loses_its_pages() {
    let pages = written(FIXTURE, &["lib"]).remove(0).1;
    let said = agree_under_edits(FIXTURE, pages, 1, || 0);
    assert_eq!(said, Err("step 0: an edit of `main` took `lib`'s pages away".to_owned()));
}

/// The control: the same run over pages whose name tables leave `Shown` out is caught, at
/// the first comparison or at an edit that names it.
#[test]
fn the_oracle_catches_a_name_left_out_under_edits() {
    let apart = apart();
    let resolved = fixture::db(&apart).into_db();
    let lib = crate_named(&resolved, "lib");
    let pair = crate_local_def_map(&resolved, lib);
    let planted =
        encode_maps(&resolved, lib, pair.def_map(&resolved), pair.local(&resolved), Some("Shown"))
            .unwrap();
    let mut at = 0;
    let said = agree_under_edits(&apart, planted, EDITS.len(), || {
        at += 1;
        at - 1
    });
    assert!(said.is_err(), "pages without `Shown` answered as the resolved crate");
}

/// The control of the refinement under edits: with each lookup serving the entry of the
/// module's first name in slot order (an entry of another name served, the planted bug), the
/// same run is caught at its first comparison.
#[test]
fn the_oracle_catches_another_names_entry_served_under_edits() {
    let apart = apart();
    let pages = written(&apart, &["lib"]).remove(0).1;
    super::SERVES_ANOTHER_NAMES_ENTRY.set(true);
    let mut at = 0;
    let said = agree_under_edits(&apart, pages, EDITS.len(), || {
        at += 1;
        at - 1
    });
    super::SERVES_ANOTHER_NAMES_ENTRY.set(false);
    assert!(said.is_err(), "lookups served other names' entries and answered as resolved");
}

/// A crate `lib` of `count` traits, each with an impl, a textual macro and a dependent: the
/// traits in scope, the textual macros and the items of its root grow with `count`.
fn sized_traits(count: usize) -> String {
    let mut text = String::from("//- /main.rs crate:main deps:lib\nfn main() {}\n");
    text.push_str("//- /lib.rs crate:lib\nmacro_rules! m0 { () => {} }\n");
    for at in 0..count {
        text.push_str(&format!("pub trait T{at} {{}}\npub struct S{at};\nimpl T{at} for S{at} {{}}\n"));
    }
    text
}

/// A textual macro's lookup reads its table's window and its entry: the same few pages
/// whether the module has 40 traits and impls or 4,000; and nothing a lookup or the traits'
/// read asks decodes the module's whole scope.
#[test]
fn a_textual_macro_reads_a_bounded_number_of_pages_and_no_scope() {
    use crate::test_db::TestDBExt;
    for count in [40, 4000] {
        let text = sized_traits(count);
        let pages = written(&text, &["lib"]).remove(0).1;
        let source = Counting::new(pages);
        // clone: a count of the test's source
        let db = paged(&text, &[("lib", source.clone())]);
        let lib = crate_named(&db, "lib");
        let def_map = crate_def_map(&db, lib);
        source.take();
        let root = def_map.module(&db, def_map.root);
        let ran = db.executed_in_read_order(|| {
            assert!(root.legacy_macro_in_scope(&db, &plain("m0")).is_some(), "{count}");
            let touched = source.take();
            assert!((1..=LOOKUP_PAGES).contains(&touched), "{count} items: {touched} pages read");
            assert!(root.legacy_macro_in_scope(&db, &plain("no_such")).is_none(), "{count}");
            let touched = source.take();
            assert!((1..=LOOKUP_PAGES).contains(&touched), "{count} items: {touched} pages read");
            assert_eq!(root.traits_in_scope(&db).count(), count);
            for module in 0..super::modules(&db, lib) {
                let items = super::module_items(&db, lib, module);
                assert_eq!(items.impls.len(), if module == 0 { count } else { 0 });
            }
        });
        assert!(!ran.iter().any(|it| it.contains("scope_memo")), "{count}: {ran:?}");
    }
}

/// The sealed crates of [`sealed_many`].
const SEALED: [&str; 8] = ["l0", "l1", "l2", "l3", "l4", "l5", "l6", "l7"];

/// Each crate of [`SEALED`] with a function, a struct and a textual macro, and `main`, from
/// source, importing one function of each by name.
fn sealed_many() -> String {
    let deps = SEALED.join(",");
    let mut text = format!("//- /main.rs crate:main deps:{deps}\n");
    for (at, name) in SEALED.iter().enumerate() {
        text.push_str(&format!("use {name}::f{at};\n"));
    }
    text.push_str("fn main() {}\n");
    for (at, name) in SEALED.iter().enumerate() {
        text.push_str(&format!(
            "//- /{name}.rs crate:{name} new_source_root:local\n\
             pub fn f{at}() {{}}\npub struct S{at};\nmacro_rules! m{at} {{ () => {{}} }}\n"
        ));
    }
    text
}

/// What resolving `main` above every crate of [`SEALED`] read from pages runs, with the
/// queries that hold a decoded part of a sealed crate's name resolution counted: `scope_memo`
/// (a module's whole scope) and `tail` (its name records). With `whole`, the planted cause:
/// `l3`'s root scope asked for whole.
fn sealed_run(whole: bool) -> Vec<String> {
    use crate::test_db::TestDBExt;
    let text = sealed_many();
    let pages = written(&text, &SEALED);
    let db = paged(&text, &sources(&pages));
    let ran = db.executed_in_read_order(|| {
        let main = crate_named(&db, "main");
        let def_map = crate_def_map(&db, main);
        for at in 0..SEALED.len() {
            let found = def_map.name_in_module(&db, def_map.root, &plain(&format!("f{at}")));
            assert!(found.values.is_some(), "`f{at}` imported into `main`: {found:?}");
        }
        for (at, name) in SEALED.iter().enumerate() {
            let krate = crate_named(&db, name);
            assert!(super::is_paged(&db, krate), "{name} is read from its pages");
            let map = crate_def_map(&db, krate);
            let root = map.module(&db, map.root);
            assert!(root.legacy_macro_in_scope(&db, &plain(&format!("m{at}"))).is_some());
            let found = map.name_in_module(&db, map.root, &plain(&format!("S{at}")));
            assert!(found.types.is_some(), "{name}: {found:?}");
            if whole && at == 3 {
                assert!(root.scope(&db).get(&plain("S3")).types.is_some());
            }
        }
    });
    // An ingredient's name is its function's: `tail` is the pages' tail, which only a crate
    // read from pages runs.
    ran.into_iter().filter(|it| it.ends_with("scope_memo") || it == "tail" || it.ends_with("::tail")).collect()
}

/// The invariant: name resolution holds no decoded state of a sealed crate. `main` resolved
/// over eight crates read from pages, each crate's names and textual macros looked up, runs
/// no query that decodes a scope or a tail of any of them: what is held of each is its
/// lookups' answers and the head (its rows and module tree), never a scope.
#[test]
fn name_resolution_state_stays_zero_across_many_sealed_crates() {
    let held = sealed_run(false);
    assert_eq!(held, Vec::<String>::new());
}

/// The control: the cause put back. One sealed crate's whole scope asked for runs one
/// `scope_memo`, which the check above sees.
#[test]
fn a_sealed_crates_whole_scope_is_seen_by_the_zero_state_check() {
    let held = sealed_run(true);
    assert_eq!(held.iter().filter(|it| it.contains("scope_memo")).count(), 1, "{held:?}");
}

/// Names written once: a module's scope segment names each of its names by its entry's
/// position, so a long name's text is in its entry and not in the scope segment; the scope
/// read whole from the two answers as the resolved crate. The control: the names part of the
/// whole form ([`Enc::scope_names`], which layout 2 wrote into every scope segment) holds the
/// name's text.
#[test]
fn a_modules_names_are_written_once() {
    let long = "n".repeat(4000);
    let text = format!("//- /main.rs crate:main deps:lib\nfn main() {{}}\n//- /lib.rs crate:lib\npub fn {long}() {{}}\n");
    let pages = written(&text, &["lib"]).remove(0).1;
    let source = Counting::new(pages);
    // clone: a count of the test's source
    let db = paged(&text, &[("lib", source.clone())]);
    let lib = crate_named(&db, "lib");
    let head = read_head(&db, lib, &*source).unwrap();
    let root = head.rows[0];
    assert!((root.scope.1 as usize) < long.len(), "the scope segment is {} bytes", root.scope.1);
    assert!((root.entries.1 as usize) > long.len(), "the entries are {} bytes", root.entries.1);
    let map = crate_def_map(&db, lib);
    assert!(map.module(&db, map.root).scope(&db).get(&plain(&long)).values.is_some());
    let resolved = fixture::db(&text).into_db();
    assert_eq!(agree(&resolved, &db, &["lib"]), Ok(()));

    let lib = crate_named(&resolved, "lib");
    let map = crate_def_map(&resolved, lib);
    let scope = map.module(&resolved, map.root).scope(&resolved);
    let mut enc = Enc::new(&resolved, lib);
    let names = enc.fingerprint(|enc| enc.scope_names(scope)).unwrap();
    assert!(names.len() > long.len(), "the names part is {} bytes", names.len());
}

/// The control of the check above: the cause put back. The module's whole scope asked for
/// runs `scope_memo`, which the check sees.
#[test]
fn a_whole_scope_asked_for_is_seen_by_the_check() {
    use crate::test_db::TestDBExt;
    let text = sized_traits(40);
    let pages = written(&text, &["lib"]).remove(0).1;
    let db = paged(&text, &[("lib", Counting::new(pages))]);
    let lib = crate_named(&db, "lib");
    let def_map = crate_def_map(&db, lib);
    let root = def_map.module(&db, def_map.root);
    let ran = db.executed_in_read_order(|| _ = root.scope(&db).legacy_macros().count());
    assert!(ran.iter().any(|it| it.contains("scope_memo")), "{ran:?}");
}
