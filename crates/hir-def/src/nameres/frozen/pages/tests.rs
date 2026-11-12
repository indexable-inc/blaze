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
        for (index, ((&id_a, data), (&id_b, _))) in table_a.iter().zip(table_b.iter()).enumerate()
        {
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
