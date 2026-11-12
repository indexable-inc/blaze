//! Tests of a crate's items read from pages: each with its control, a planted bug or the
//! cause removed.

use std::sync::Arc;

use base_db::{Crate, Db};
use blaze_core::lock::Lock;
use blaze_hash::HashSet;
use blaze_ids::ContentId;
use hir_expand::fixture;
use std::alloc::Global;

use super::{
    Asked, Enc, HEADER, LAYOUT, PageSource, Paged, asked, encode, encode_with, read_head, set,
};
use crate::{
    AssocItemId, AttrDefId, ModuleDefId,
    attrs::AttrFlags,
    nameres::{
        assoc::{ImplItems, TraitItems},
        crate_def_map,
        frozen::{pages, tests::crate_named},
    },
};

/// A dependency with what the record holds: a trait with each kind of assoc item, impls
/// written in the file and by a macro, assoc items a macro call writes, attributes on items,
/// variants and assoc items, an inline module; and a dependent.
const FIXTURE: &str = r#"
//- /main.rs crate:main deps:lib
use lib::{Area, Shape};
fn main() { let _ = Shape::unit().area(); }
//- /lib.rs crate:lib
macro_rules! methods { ($($name:ident)*) => { $(pub fn $name(&self) -> u32 { 0 })* } }
macro_rules! impls { ($($ty:ident)*) => { $(impl Area for $ty { type Unit = u8; fn area(&self) -> u32 { 1 } })* } }
pub trait Area {
    const SIDES: u32 = 0;
    type Unit;
    fn area(&self) -> u32;
    #[inline]
    fn twice(&self) -> u32 { 2 }
}
#[repr(C)]
pub struct Shape { pub side: u32 }
pub struct Other;
impl Shape {
    #[inline]
    pub fn unit() -> Shape { Shape { side: 1 } }
    methods!(first second);
}
impls!(Other);
impl Area for Shape { type Unit = u32; fn area(&self) -> u32 { self.side } }
#[deprecated]
pub fn old() {}
pub mod deep {
    #[must_use]
    pub fn f() -> u32 { 0 }
    pub enum E { #[non_exhaustive] A, B }
}
const _: () = ();
"#;

/// A page of the store's pool.
const PAGE: u64 = 4096;

/// A record in memory that notes each page a read touches (as `pages::tests`' own, which is
/// private to that module).
struct Counting {
    bytes: Vec<u8>,
    touched: Lock<HashSet<u64, Global>>,
}

impl Counting {
    fn new(bytes: Vec<u8>) -> Arc<Counting> {
        let touched = Lock::new("test.item.pages.touched", HashSet::new_in(Global));
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

/// The def-map pages and the item pages of `lib`, written by a fresh database of `text`.
fn written(text: &str) -> (Vec<u8>, Vec<u8>) {
    let db = fixture::db(text).into_db();
    let lib = crate_named(&db, "lib");
    (pages::encode(&db, lib).unwrap(), encode(&db, lib).unwrap())
}

/// A database of `text` that reads the `DefMap` of `lib` from `def_map` and, when given, its
/// items from `items`.
fn reading(text: &str, def_map: Vec<u8>, items: Option<Arc<Counting>>) -> Db {
    let mut db = fixture::db(text).into_db();
    let lib = crate_named(&db, "lib");
    let paged = Paged { key: ContentId([1; 32]), source: Counting::new(def_map) };
    pages::set(&mut db, [(lib, paged)].into_iter().collect());
    if let Some(source) = items {
        let paged = Paged { key: ContentId([2; 32]), source };
        set(&mut db, [(lib, paged)].into_iter().collect());
    }
    db
}

/// Every answer the record holds of `krate`, asked of `db` through the queries themselves,
/// by the entry's key, with nothing of one database in it: two databases' answers compare by
/// it.
fn answers(db: &Db, krate: Crate) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut enc = Enc::new(db, krate);
    let mut all: Vec<(Vec<u8>, Vec<u8>)> = asked(db, krate)
        .into_iter()
        .map(|asked| {
            let print = enc
                .fingerprint(|enc| match asked {
                    Asked::Impl(it) => {
                        let (items, diagnostics) = ImplItems::of(db, it);
                        enc.assoc(&items.items, &items.macro_calls, diagnostics, None)
                    }
                    Asked::Trait(it) => {
                        let (items, diagnostics) = TraitItems::query_with_diagnostics(db, it);
                        enc.assoc(&items.items, &items.macro_calls, diagnostics, None)
                    }
                    Asked::Flags(it) => {
                        enc.flags(AttrFlags::query(db, it));
                        Ok(())
                    }
                })
                .unwrap();
            (asked.key(db, krate), print)
        })
        .collect();
    let mut includes: Vec<Vec<u8>> = crate::include_macro_invoc(db, krate)
        .iter()
        .map(|&(call, _)| enc.fingerprint(|enc| enc.call(call)).unwrap())
        .collect();
    includes.sort_unstable();
    all.push((b"includes".to_vec(), includes.concat()));
    all.sort_unstable();
    all
}

/// The refinement: `read` answers every query the record holds of `lib` as `lowered`, which
/// made them from source; `Err` names the first key that differs.
fn agree(lowered: &Db, read: &Db) -> Result<usize, String> {
    let was = answers(lowered, crate_named(lowered, "lib"));
    let is = answers(read, crate_named(read, "lib"));
    if was.len() != is.len() {
        return Err(format!("{} answers, {} read", was.len(), is.len()));
    }
    for (a, b) in was.iter().zip(&is) {
        if a != b {
            return Err(format!("the answer of key {:02x?} is {:02x?}, read {:02x?}", a.0, a.1, b.1));
        }
    }
    Ok(was.len())
}

/// The refinement: a crate whose items are read from pages answers `ImplItems::of`,
/// `TraitItems::query_with_diagnostics`, `AttrFlags::query` and `include_macro_invoc` as the
/// crate lowered from source does.
#[test]
fn a_crate_read_from_item_pages_answers_as_the_one_lowered() {
    let lowered = fixture::db(FIXTURE).into_db();
    let (def_map, items) = written(FIXTURE);
    let read = reading(FIXTURE, def_map, Some(Counting::new(items)));
    let compared = agree(&lowered, &read).unwrap();
    // The comparison is of something: the trait, four impls' worth of items and their flags.
    assert!(compared > 20, "{compared} answers compared");
    // And the answers are not all one: an `#[inline]` method's flags are not a plain one's.
    let lib = crate_named(&lowered, "lib");
    let flags: HashSet<Vec<u8>, Global> = answers(&lowered, lib).into_iter().map(|it| it.1).collect();
    assert!(flags.len() > 4, "{} distinct answers", flags.len());
}

/// The control: the same oracle over a record whose impls and traits leave `area` out.
#[test]
fn the_oracle_catches_an_assoc_item_left_out_of_its_entry() {
    let lowered = fixture::db(FIXTURE).into_db();
    let lib = crate_named(&lowered, "lib");
    let (def_map, _) = written(FIXTURE);
    let planted = encode_with(&lowered, lib, Some("area")).unwrap();
    let read = reading(FIXTURE, def_map, Some(Counting::new(planted)));
    let said = agree(&lowered, &read).unwrap_err();
    assert!(said.contains("the answer of key"), "{said}");
}

/// Asks of `lib` what a dependent's build asks of a dependency's items once its names are
/// found: each impl's and trait's items, and the flags of each item and assoc item its scopes
/// declare. How many were asked. Nothing here is found through syntax: the ids are the
/// scopes'.
fn ask_all(db: &Db) -> usize {
    let lib = crate_named(db, "lib");
    let mut asked = 0;
    let assoc = |items: &[(hir_expand::name::Name, AssocItemId)]| {
        for &(_, item) in items {
            _ = AttrFlags::query(
                db,
                match item {
                    AssocItemId::FunctionId(it) => AttrDefId::FunctionId(it),
                    AssocItemId::ConstId(it) => AttrDefId::ConstId(it),
                    AssocItemId::TypeAliasId(it) => AttrDefId::TypeAliasId(it),
                },
            );
        }
        items.len()
    };
    for (_, data) in crate_def_map(db, lib).table(db).iter() {
        let scope = data.scope(db);
        for def in scope.declarations() {
            let owner = match def {
                ModuleDefId::FunctionId(it) => AttrDefId::FunctionId(it),
                ModuleDefId::AdtId(it) => AttrDefId::AdtId(it),
                ModuleDefId::TraitId(it) => {
                    asked += assoc(&TraitItems::query_with_diagnostics(db, it).0.items);
                    AttrDefId::TraitId(it)
                }
                _ => continue,
            };
            _ = AttrFlags::query(db, owner);
            asked += 1;
        }
        for it in scope.impls() {
            asked += assoc(&ImplItems::of(db, it).0.items);
            _ = AttrFlags::query(db, AttrDefId::ImplId(it));
            asked += 1;
        }
    }
    asked
}

/// The queries among `ran` that make syntax, an item tree or an expansion.
fn made_syntax(ran: &[String]) -> Vec<&String> {
    const MAKERS: &[&str] =
        &["parse", "item_tree", "macro_expand", "outline", "expander", "macro_arg", "ast_id_map"];
    ran.iter().filter(|name| MAKERS.iter().any(|it| name.contains(it))).collect()
}

/// The count: a crate read from its def-map pages and its item pages has its impls', traits'
/// and items' answers asked, and no file of it is parsed, no item tree made, no macro
/// expanded and no outline cut.
#[test]
fn a_crate_read_from_item_pages_makes_no_syntax_tree_or_expansion() {
    use crate::test_db::TestDBExt;
    let (def_map, items) = written(FIXTURE);
    let db = reading(FIXTURE, def_map, Some(Counting::new(items)));
    let mut asked = 0;
    let ran = db.executed_in_read_order(|| asked = ask_all(&db));
    assert!(asked > 12, "{asked} asked");
    assert_eq!(made_syntax(&ran), Vec::<&String>::new(), "all that ran: {ran:?}");
}

/// The control: the cause removed. The same crate read from its def-map pages alone parses
/// and expands to answer the same asks.
#[test]
fn without_item_pages_the_same_asks_parse_and_expand() {
    use crate::test_db::TestDBExt;
    let (def_map, _) = written(FIXTURE);
    let db = reading(FIXTURE, def_map, None);
    let ran = db.executed_in_read_order(|| _ = ask_all(&db));
    let made = made_syntax(&ran);
    assert!(made.iter().any(|it| it.contains("parse")), "{ran:?}");
    assert!(made.iter().any(|it| it.contains("macro_expand")), "{ran:?}");
}

/// Two databases that asked for things in another order, and so numbered their ids
/// otherwise, write the same bytes.
#[test]
fn the_same_item_pages_from_two_databases() {
    let (_, first) = written(FIXTURE);
    let other = fixture::db(FIXTURE).into_db();
    let (main, lib) = (crate_named(&other, "main"), crate_named(&other, "lib"));
    // The dependent first, and the dependency's items asked last to first.
    _ = pages::encode(&other, main).unwrap();
    for asked in asked(&other, lib).into_iter().rev() {
        match asked {
            Asked::Impl(it) => _ = ImplItems::of(&other, it),
            Asked::Trait(it) => _ = TraitItems::query_with_diagnostics(&other, it),
            Asked::Flags(it) => _ = AttrFlags::query(&other, it),
        }
    }
    let second = encode(&other, lib).unwrap();
    assert_eq!(first, second);
    assert!(first.len() > HEADER);
}

/// The control of the comparison above: one attribute changed in the source is other bytes.
#[test]
fn the_item_pages_of_another_source_are_other_bytes() {
    let changed = FIXTURE.replace("#[deprecated]", "#[must_use]");
    assert_ne!(changed, FIXTURE);
    assert_ne!(written(FIXTURE).1, written(&changed).1);
}

/// Why `bytes`, given as the item pages of `lib`, do not read; `None` when they do.
fn refused(bytes: Vec<u8>) -> Option<String> {
    let (def_map, _) = written(FIXTURE);
    let source = Counting::new(bytes);
    // clone: a count of the test's source
    let db = reading(FIXTURE, def_map, Some(source.clone()));
    read_head(&db, crate_named(&db, "lib"), &*source).err()
}

/// A record of a previous layout, of another encoding or of another kind is refused by name,
/// and so is one cut short; the record as written reads (the control).
#[test]
fn a_previous_or_foreign_item_layout_is_refused_by_name() {
    let (def_map, items) = written(FIXTURE);
    // clone: a test's copies of a record
    assert_eq!(refused(items.clone()), None);

    let mut previous = items.clone();
    previous[8..12].copy_from_slice(&(LAYOUT + 1).to_le_bytes());
    let said = refused(previous).unwrap();
    assert!(said.contains(&format!("layout {}", LAYOUT + 1)), "{said}");

    let mut encoding = items.clone();
    encoding[16] ^= 1;
    let said = refused(encoding).unwrap();
    assert!(said.contains("encoding"), "{said}");

    assert!(def_map.len() > HEADER);
    let said = refused(def_map).unwrap();
    assert!(said.contains("no item pages"), "{said}");

    let mut cut = items;
    cut.pop();
    let said = refused(cut).unwrap();
    assert!(said.contains("header says"), "{said}");
}

/// A crate `lib` of `count` structs, each with an impl of two methods, and a dependent.
fn sized(count: usize) -> String {
    let mut text = String::from("//- /main.rs crate:main deps:lib\nfn main() {}\n");
    text.push_str("//- /lib.rs crate:lib\n");
    for at in 0..count {
        text.push_str(&format!(
            "pub struct S{at};\nimpl S{at} {{ pub fn f(&self) {{}} #[inline] pub fn g(&self) {{}} }}\n"
        ));
    }
    text
}

/// The most pages one entry's lookup reads: a window of slots (two pages where it crosses
/// one, a third where it wraps) and the entry (two where it crosses one), and one more for a
/// key whose slot is past its window.
const LOOKUP_PAGES: usize = 6;

/// A lookup of one impl's items and of one method's flags reads the pages they are on: the
/// same few whether the crate is 40 impls or 4,000.
#[test]
fn an_items_lookup_reads_a_bounded_number_of_pages() {
    let mut records = Vec::new();
    for count in [40, 4000] {
        let text = sized(count);
        let (def_map, items) = written(&text);
        let record_pages = items.len().div_ceil(PAGE as usize);
        let source = Counting::new(items);
        // clone: a count of the test's source
        let db = reading(&text, def_map, Some(source.clone()));
        let lib = crate_named(&db, "lib");
        let def_map = crate_def_map(&db, lib);
        let impls: Vec<_> = def_map.module(&db, def_map.root).scope(&db).impls().collect();
        assert_eq!(impls.len(), count);
        // The head, read once a crate.
        _ = ImplItems::of(&db, impls[0]);
        source.take();
        let (items, _) = ImplItems::of(&db, impls[count / 2]);
        assert_eq!(items.items.len(), 2, "{count}");
        let touched = source.take();
        assert!((1..=LOOKUP_PAGES).contains(&touched), "{count} impls: {touched} pages read");
        let AssocItemId::FunctionId(method) = items.items[1].1 else { panic!("a method") };
        _ = AttrFlags::query(&db, AttrDefId::FunctionId(method));
        let touched = source.take();
        assert!((1..=LOOKUP_PAGES).contains(&touched), "{count} impls: {touched} pages read");
        records.push(record_pages);
    }
    // The larger record is far over the bound, so the bound is the lookup's, not the record's.
    assert!(records[1] > 10 * LOOKUP_PAGES, "{records:?}");
    assert!(records[1] > 10 * records[0], "{records:?}");
}

/// The control: the cause removed. Every impl's items asked for reads far more pages than
/// the bound.
#[test]
fn every_impls_items_read_more_pages_than_one() {
    let text = sized(4000);
    let (def_map, items) = written(&text);
    let source = Counting::new(items);
    // clone: a count of the test's source
    let db = reading(&text, def_map, Some(source.clone()));
    let lib = crate_named(&db, "lib");
    let def_map = crate_def_map(&db, lib);
    let impls: Vec<_> = def_map.module(&db, def_map.root).scope(&db).impls().collect();
    for &it in &impls {
        _ = ImplItems::of(&db, it);
    }
    let touched = source.take();
    assert!(touched > 2 * LOOKUP_PAGES, "{touched} pages read");
}
