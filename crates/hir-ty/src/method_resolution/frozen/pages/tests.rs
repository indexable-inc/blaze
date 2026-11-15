//! Tests of a crate's impl tables read from pages: each with its control, a planted bug or
//! the cause removed.

use std::sync::Arc;

use base_db::{Crate, Db, all_crates};
use blaze_core::lock::Lock;
use blaze_hash::HashSet;
use blaze_mem::Tag;
use either::Either;
use hir_def::{
    BuiltinDeriveImplId, ImplId, ModuleDefId, TraitId,
    deps::{Mode, PageSource},
    nameres::frozen::{Enc, Unreadable},
};
use hir_expand::{fixture, name::Name};
use intern::Symbol;
use rustc_type_ir::fast_reject::SimplifiedType as S;
use std::alloc::Global;

use super::{
    HEADER, SCANS_FOR_ABSENT, WRITES_NO_ROW_SLOT, WRITES_NO_SELF_SLOT, encode, read_head,
};
use crate::{
    method_resolution::{
        CandidateId, InherentImpls, SelfTyKey, TraitImpls,
        frozen::{self, Tables, write_simplified},
        in_crate_trait_value_items, trait_value_items,
    },
    next_solver::SimplifiedType,
};

/// A page of the store's pool.
const PAGE: u64 = 4096;

/// The most pages one lookup reads: a window of slots (two pages where it crosses one, a
/// third where it wraps) and the entry (two where it crosses one), and one more for a key
/// whose slot is past its window.
const LOOKUP_PAGES: usize = 6;

const FIXTURE: &str = r#"
//- minicore: derive, clone, copy, sized
//- /main.rs crate:main deps:lib
use lib::{Show, Wrap};
struct Local(u8);
impl Show for Local {}
impl Local { fn new() -> Self { Local(0) } }
fn main() { let _ = Wrap(Local::new()).get(); }

//- /lib.rs crate:lib
pub trait Show { fn show(&self) {} const N: u8 = 0; }
pub trait Any { fn any(&self) {} fn show(&self) {} }
pub trait Unused { fn never(&self); }
#[derive(Clone, Copy)]
pub struct Wrap<T>(pub T);
pub struct Plain;
pub enum Choice { A, B }
impl Show for u8 {}
impl Show for i128 {}
impl Show for f64 {}
impl Show for bool {}
impl Show for char {}
impl Show for &str {}
impl Show for &mut u8 {}
impl<T> Show for [T] {}
impl<T, const N: usize> Show for [T; N] {}
impl<T> Show for *const T {}
impl<T> Show for *mut T {}
impl Show for (u8, u16) {}
impl Show for fn(u8) -> u8 {}
impl Show for dyn Any {}
impl Show for Plain {}
impl Show for Choice {}
impl<T: Show> Show for Wrap<T> {}
impl<T> Any for T {}
impl<T> Wrap<T> { pub fn get(self) -> T { self.0 } }
impl Plain { pub fn new() -> Plain { Plain } }
impl Plain { pub fn other(&self) {} }
impl Choice { pub fn pick(&self) {} }
pub mod inner { pub struct Deep; impl super::Show for Deep {} impl Deep { pub fn f(&self) {} } }
pub fn holds() { struct InBody; impl Show for InBody {} impl InBody { fn f() {} } }
"#;

/// A record in memory that notes each page a read touches.
struct Counting {
    bytes: Vec<u8>,
    touched: Lock<HashSet<u64, Global>>,
}

impl Counting {
    fn new(bytes: Vec<u8>) -> Arc<Counting> {
        let touched = Lock::new("test.ty.impl_pages.touched", HashSet::new_in(Global));
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

fn named(db: &Db, name: &str) -> Crate {
    *all_crates(db)
        .iter()
        .find(|it| it.named(db).to_string() == name)
        .unwrap_or_else(|| panic!("no crate `{name}`"))
}

/// Runs `f` as a query's caller does: the database attached, in a task's frame.
fn within<T>(db: &Db, f: impl FnOnce() -> T) -> T {
    blaze_query::db::attach(db, || blaze_mem::frame::task(Tag::Infer, |_| f()))
}

/// The pages of the impl tables of `name`, written by a fresh database of `text`.
fn written(text: &str, name: &str) -> Vec<u8> {
    let db = fixture::db(text).into_db();
    within(&db, || encode(&db, named(&db, name)).expect("the impls encode"))
}

/// A database of `text` that reads the impls of `name` from `source`.
fn paged(text: &str, name: &str, source: Arc<Counting>) -> Db {
    let mut db = fixture::db(text).into_db();
    let krate = named(&db, name);
    let mut tables = Tables::whole(&[], Mode::Serve);
    tables.pages.source = source;
    frozen::set(&mut db, [(krate, tables)].into_iter().collect());
    db
}

/// What `write` writes, with nothing of one database in it: two databases' answers compare
/// by it.
fn print<'a>(
    db: &'a Db,
    krate: Crate,
    write: impl FnOnce(&mut Enc<'a>) -> Result<(), String>,
) -> Vec<u8> {
    let mut enc = Enc::new(db, krate);
    enc.fingerprint(write).unwrap()
}

fn print_row(
    db: &Db,
    krate: Crate,
    trait_: TraitId,
    self_ty: &SimplifiedType<'_>,
    impls: &[ImplId],
    derives: &[BuiltinDeriveImplId],
) -> Vec<u8> {
    print(db, krate, |enc| {
        enc.str("row")?;
        enc.def(ModuleDefId::TraitId(trait_))?;
        write_simplified(enc, self_ty)?;
        enc.usize(impls.len())?;
        for &it in impls {
            enc.impl_id(it)?;
        }
        enc.usize(derives.len())?;
        for &it in derives {
            enc.builtin_derive_impl(it)?;
        }
        Ok(())
    })
}

fn print_impls(
    db: &Db,
    krate: Crate,
    what: &str,
    key: impl FnOnce(&mut Enc<'_>) -> Result<(), String>,
    impls: &[ImplId],
) -> Vec<u8> {
    print(db, krate, |enc| {
        enc.str(what)?;
        key(enc)?;
        enc.usize(impls.len())?;
        for &it in impls {
            enc.impl_id(it)?;
        }
        Ok(())
    })
}

fn print_item(db: &Db, krate: Crate, name: Name, trait_: TraitId, item: CandidateId) -> Vec<u8> {
    print(db, krate, |enc| {
        enc.str("item")?;
        enc.name(&name)?;
        enc.def(ModuleDefId::TraitId(trait_))?;
        enc.def(match item {
            CandidateId::FunctionId(it) => ModuleDefId::FunctionId(it),
            CandidateId::ConstId(it) => ModuleDefId::ConstId(it),
        })
    })
}

fn plain(text: &str) -> Name {
    Name::new_symbol_root(Symbol::intern(text))
}

/// Every answer about the impls of `krate`, each as its print, sorted. The keys asked are
/// those of the crate's whole tables. With `narrow`, each answer is from the reader of one
/// key (a row, a trait's blanket impls, a type's inherent impls, a name's items), which for
/// a crate read from pages reads that key's entry; without, from the whole tables. With
/// `narrow` the readers of one key are also held to each other, and keys the crate has
/// nothing for are asked and must answer nothing.
fn answers(db: &Db, krate: Crate, narrow: bool) -> Vec<Vec<u8>> {
    within(db, || {
        let mut out = Vec::new();
        let table = &**TraitImpls::for_crate(db, krate);
        for (trait_, t) in table.trait_indices() {
            let blanket: Vec<ImplId> = match narrow {
                true => TraitImpls::served_blanket(db, krate, trait_).blanket_impls(trait_).to_vec(),
                false => table.blanket_impls(trait_).to_vec(),
            };
            let key = |enc: &mut Enc<'_>| enc.def(ModuleDefId::TraitId(trait_));
            out.push(print_impls(db, krate, "blanket", key, &blanket));
            let of_trait = TraitImpls::for_crate_and_trait(db, krate, trait_);
            for (self_ty, (impls, derives)) in table.rows_at(t) {
                // Every trait impl for the row's type: from the type's entry and its rows, or
                // from the whole table, as `hir`'s `Impl::all_for_type` asks.
                let mut of_self = Vec::new();
                let mut push = |it: Either<ImplId, BuiltinDeriveImplId>| {
                    of_self.push(print(db, krate, |enc| {
                        enc.str("for self")?;
                        write_simplified(enc, self_ty)?;
                        match it {
                            Either::Left(it) => enc.impl_id(it),
                            Either::Right(it) => enc.builtin_derive_impl(it),
                        }
                    }))
                };
                match narrow {
                    true => super::for_self_ty(db, krate, self_ty, &mut push),
                    false => table.for_self_ty(self_ty, |run| match run {
                        Either::Left(impls) => impls.iter().for_each(|&it| push(Either::Left(it))),
                        Either::Right(derives) => {
                            derives.iter().for_each(|&it| push(Either::Right(it)))
                        }
                    }),
                }
                out.extend(of_self);
                if !narrow {
                    out.push(print_row(db, krate, trait_, self_ty, impls, derives));
                    continue;
                }
                let (mut found, mut found_derives) = (Vec::new(), Vec::new());
                frozen::row(db, krate, trait_, self_ty, |it| match it {
                    Either::Left(it) => found.push(it),
                    Either::Right(it) => found_derives.push(it),
                });
                // The readers of one row agree: the entry, the memo over it, the trait's table.
                let memo = TraitImpls::in_crate_for_trait_and_self_ty(
                    db,
                    krate,
                    trait_,
                    SelfTyKey(*self_ty),
                );
                assert_eq!(memo, &found[..], "{trait_:?} for {self_ty:?}");
                let (in_trait, derives_in_trait) = of_trait.for_trait_and_self_ty(trait_, self_ty);
                assert_eq!(in_trait, &found[..], "{trait_:?} for {self_ty:?}, the trait's table");
                assert_eq!(derives_in_trait, &found_derives[..], "{trait_:?} for {self_ty:?}");
                out.push(print_row(db, krate, trait_, self_ty, &found, &found_derives));
            }
            if narrow {
                // A type the trait has no impl for: nothing, from the row's slot.
                let absent = SelfTyKey(S::Tuple(9));
                let none = TraitImpls::in_crate_for_trait_and_self_ty(db, krate, trait_, absent);
                assert!(none.is_empty(), "{trait_:?} for a tuple of nine: {none:?}");
                // A type no trait has an impl for: nothing, from the type's slot.
                super::for_self_ty(db, krate, &S::Tuple(9), |it| {
                    panic!("a trait impl for a tuple of nine: {it:?}")
                });
            }
        }
        let inherent = InherentImpls::for_crate(db, krate);
        for (self_ty, impls) in inherent.iter() {
            let found: Vec<ImplId> = match narrow {
                true => InherentImpls::in_crate_for_self_ty(db, krate, SelfTyKey(*self_ty)).to_vec(),
                false => impls.to_vec(),
            };
            let key = |enc: &mut Enc<'_>| write_simplified(enc, self_ty);
            out.push(print_impls(db, krate, "inherent", key, &found));
        }
        let items = trait_value_items(db, krate);
        if narrow {
            let mut names: Vec<Name> = items.iter().map(|it| it.0).collect();
            names.dedup();
            for name in names {
                for &(trait_, item) in in_crate_trait_value_items(db, krate, name) {
                    out.push(print_item(db, krate, name, trait_, item));
                }
            }
            let absent = SelfTyKey(S::Tuple(9));
            assert!(InherentImpls::in_crate_for_self_ty(db, krate, absent).is_empty());
            let none = in_crate_trait_value_items(db, krate, plain("no_trait_has_this"));
            assert!(none.is_empty(), "{none:?}");
        } else {
            for &(name, trait_, item) in items {
                out.push(print_item(db, krate, name, trait_, item));
            }
        }
        out.sort();
        out
    })
}

/// Whether a database of `text` that reads the impls of `lib` from `pages` answers as one
/// that finds them: every key's answer from its own reader against the finder's whole
/// tables, and the whole tables written again against `pages`.
fn agree(text: &str, pages: &[u8]) -> Result<(), String> {
    let found = fixture::db(text).into_db();
    // clone: a test's copy of a record
    let read = paged(text, "lib", Counting::new(pages.to_vec()));
    let (a, b) = (named(&found, "lib"), named(&read, "lib"));
    assert!(frozen::served(&read, b) && !frozen::served(&found, a));
    let reference = answers(&found, a, false);
    if reference.len() < 20 {
        return Err(format!("only {} answers were compared", reference.len()));
    }
    if answers(&read, b, true) != reference {
        return Err("read from pages, a key's reader answers otherwise".to_owned());
    }
    if answers(&read, b, false) != reference {
        return Err("read from pages, the whole tables are others".to_owned());
    }
    if within(&read, || encode(&read, b))? != pages {
        return Err("read from pages and written again, the record is another".to_owned());
    }
    // The member over the crate read from pages: its own tables are found as ever.
    let (main_a, main_b) = (named(&found, "main"), named(&read, "main"));
    if answers(&read, main_b, false) != answers(&found, main_a, false) {
        return Err("`main` over a crate read from pages answers otherwise".to_owned());
    }
    Ok(())
}

/// The refinement: a database that reads a crate's impls from its pages answers every query
/// of them as one that found them from source: each row, each trait's blanket impls and
/// whole table, each type's inherent impls, each name's trait items, by the reader of that
/// key alone; keys the crate has nothing for answer nothing; and the whole tables read back
/// write the same record (the round trip of every entry).
///
/// Its control is [`the_oracle_catches_rows_written_with_no_slot`].
#[test]
fn a_crate_read_from_impl_pages_answers_as_the_one_that_found_its_impls() {
    agree(FIXTURE, &written(FIXTURE, "lib")).unwrap();
}

/// The control: with every row written and none given a slot (the planted bug), a row's
/// reader finds nothing where the finder has impls, which the oracle says.
#[test]
fn the_oracle_catches_rows_written_with_no_slot() {
    WRITES_NO_ROW_SLOT.set(true);
    let pages = written(FIXTURE, "lib");
    WRITES_NO_ROW_SLOT.set(false);
    let caught = std::panic::catch_unwind(|| agree(FIXTURE, &pages));
    // Either the readers of one row disagree (an assertion) or the answers differ.
    assert!(!matches!(caught, Ok(Ok(()))), "the oracle passed rows no reader can find");
}

/// A crate that declares a `#[blaze::sealed]` trait, and a member.
const SEALED: &str = r#"
//- minicore: sized, copy, clone
//- /main.rs crate:main deps:lib
fn main() {}
//- /lib.rs crate:lib
#[blaze::sealed]
pub trait Closed {}
pub struct Mine;
impl Closed for Mine {}
"#;

/// Whether a served crate declares a sealed trait is its pages' header: what the walk of the
/// crate resolved from source says, read with no module scope and no table.
#[test]
fn a_served_crate_says_from_its_header_whether_it_declares_a_sealed_trait() {
    for (text, declares) in [(SEALED, true), (FIXTURE, false)] {
        let found = fixture::db(text).into_db();
        let walked = within(&found, || crate::traits::declares_sealed(&found, named(&found, "lib")));
        assert_eq!(walked, declares);
        let read = paged(text, "lib", Counting::new(written(text, "lib")));
        let lib = named(&read, "lib");
        assert!(frozen::served(&read, lib));
        assert_eq!(within(&read, || crate::traits::declares_sealed(&read, lib)), declares);
    }
}

/// The control: the cause removed. Pages written of the crate without the sealed trait,
/// given to a database whose source has it, answer as the pages say, not as the source:
/// the answer is the header's, and no walk of the source stands in.
#[test]
fn a_served_crates_sealed_answer_is_its_pages_not_its_source() {
    let read = paged(SEALED, "lib", Counting::new(written(FIXTURE, "lib")));
    let lib = named(&read, "lib");
    assert!(!within(&read, || crate::traits::declares_sealed(&read, lib)));
}

/// The control of the self types' table: with every self type written and none given a
/// slot (the planted bug), the reader of one type's trait impls finds none where the whole
/// table has them, which the oracle says.
#[test]
fn the_oracle_catches_self_types_written_with_no_slot() {
    WRITES_NO_SELF_SLOT.set(true);
    let pages = written(FIXTURE, "lib");
    WRITES_NO_SELF_SLOT.set(false);
    let caught = std::panic::catch_unwind(|| agree(FIXTURE, &pages));
    assert!(!matches!(caught, Ok(Ok(()))), "the oracle passed self types no reader can find");
}

/// The refinement under edits elsewhere: after each of a run of edits to the member (an
/// impl for a type of the dependency added, a local type and its impls added, text only),
/// a database that reads `lib` from the pages written before the edits answers as one that
/// finds everything from the edited text. Each edit is a fresh pair of databases: an edit
/// in place of a crate read from tables is `delta::tests::tables`'s.
#[test]
fn a_crate_read_from_impl_pages_answers_alike_under_edits_elsewhere() {
    let pages = written(FIXTURE, "lib");
    let edits = [
        "impl Show for Local {}",
        "impl Show for Local {}\nimpl lib::Unused for Local { fn never(&self) {} }",
        "impl Show for Local {}\nstruct More; impl Show for More {} impl More { fn m(&self) {} }",
        "impl Show for Local {}\n// a comment\nfn other() { struct InMain; impl Show for InMain {} }",
        "impl Show for Local {}\ntrait Mine { fn show(&self) {} }\nimpl Mine for lib::Plain {}",
    ];
    // A fixed shuffle: the order of the edits is no part of the claim.
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    for _ in 0..edits.len() * 2 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let edit = edits[(state % edits.len() as u64) as usize];
        let text = FIXTURE.replace("impl Show for Local {}", edit);
        agree(&text, &pages).unwrap_or_else(|why| panic!("after `{edit}`: {why}"));
    }
}

/// Two databases of the same sources write the same pages: nothing in the record is one
/// database's id or order.
#[test]
fn the_same_impl_pages_from_two_databases() {
    let one = written(FIXTURE, "lib");
    // The second database interns other things first, so its ids are numbered otherwise.
    let db = fixture::db(FIXTURE).into_db();
    let main = named(&db, "main");
    _ = within(&db, || encode(&db, main).unwrap());
    _ = answers(&db, main, false);
    let two = within(&db, || encode(&db, named(&db, "lib")).unwrap());
    assert!(one == two, "the pages of `lib` differ between two databases");
    assert!(one.len() > HEADER);
}

/// The control: the pages are of the source. An impl more is other bytes.
#[test]
fn the_impl_pages_of_another_source_are_other_bytes() {
    let one = written(FIXTURE, "lib");
    let other = FIXTURE.replace("impl Show for bool {}", "impl Show for bool {}\nimpl Show for u64 {}");
    assert!(one != written(&other, "lib"));
}

fn refused(bytes: Vec<u8>) -> String {
    let db = fixture::db(FIXTURE).into_db();
    let source = Counting::new(bytes);
    within(&db, || read_head(&db, named(&db, "lib"), &*source)).unwrap_err()
}

/// A record of a previous or a foreign layout is refused, naming what it is: never read as
/// this layout, and never answered from source in its place.
#[test]
fn a_previous_or_foreign_impl_page_layout_is_refused_by_name() {
    let pages = written(FIXTURE, "lib");

    // clone: a test's copy of a record
    let mut previous = pages.clone();
    previous[8..12].copy_from_slice(&0u32.to_le_bytes());
    let why = refused(previous.clone());
    assert!(why.contains("impl table pages of layout 0, where this blaze reads layout 2"), "{why}");

    // The previous layout, which had no table of self types.
    // clone: as above
    let mut one = pages.clone();
    one[8..12].copy_from_slice(&1u32.to_le_bytes());
    let why = refused(one);
    assert!(why.contains("impl table pages of layout 1, where this blaze reads layout 2"), "{why}");

    // A flag of the crate this blaze does not know.
    // clone: as above
    let mut flagged = pages.clone();
    flagged[HEADER - 1] |= 0x80;
    let why = refused(flagged);
    assert!(why.contains("impl table pages with flags"), "{why}");

    // clone: as above
    let mut foreign = pages.clone();
    foreign[..8].copy_from_slice(b"blzdmpg\0");
    let why = refused(foreign);
    assert!(why.contains("no impl table pages: the record starts with"), "{why}");

    // clone: as above
    let mut encoded = pages.clone();
    encoded[16] ^= 1;
    let why = refused(encoded);
    assert!(why.contains("whose entries are of encoding"), "{why}");

    // clone: as above
    let mut cut = pages.clone();
    cut.truncate(pages.len() - 1);
    let why = refused(cut);
    assert!(why.contains("bytes whose header says"), "{why}");

    // Through a reader: the query that asks unwinds, naming the crate and the kind.
    let db = paged(FIXTURE, "lib", Counting::new(previous));
    let lib = named(&db, "lib");
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        within(&db, || {
            InherentImpls::in_crate_for_self_ty(&db, lib, SelfTyKey(S::Bool)).len()
        })
    }));
    let payload = caught.expect_err("a record of another layout was read");
    let unreadable = Unreadable::of_payload(&*payload).expect("the payload is `Unreadable`");
    assert_eq!(unreadable.krate, lib);
    assert_eq!(unreadable.kind, "impl table pages");
    assert!(unreadable.why.contains("layout 0"), "{}", unreadable.why);
}

/// A crate `lib` of `count` types, each with an impl of a trait and an inherent impl, and
/// `count` traits, each with a function of its own name.
fn sized(count: usize) -> String {
    let mut text = String::from("//- /main.rs crate:main deps:lib\nfn main() {}\n");
    text.push_str("//- /lib.rs crate:lib\npub trait Show { fn show(&self) {} }\n");
    for at in 0..count {
        text.push_str(&format!(
            "pub struct S{at};\nimpl Show for S{at} {{}}\nimpl S{at} {{ pub fn f{at}(&self) {{}} }}\n\
             pub trait T{at} {{ fn m{at}(&self) {{}} }}\n"
        ));
    }
    text
}

/// What one lookup of each kind is asked of: a trait with rows, a self type with a row and
/// an inherent impl, and a name a trait binds.
struct Asked<'db> {
    trait_: TraitId,
    self_ty: SimplifiedType<'db>,
    name: Name,
}

/// `lib` of `count` types read from pages, its keys known and nothing of a lookup read yet:
/// the database, the source, and how many pages the record is.
fn sized_and_read(count: usize) -> (Db, Arc<Counting>, usize) {
    let text = sized(count);
    let pages = written(&text, "lib");
    let record_pages = pages.len().div_ceil(PAGE as usize);
    let source = Counting::new(pages);
    // clone: a count of the test's source
    let db = paged(&text, "lib", source.clone());
    (db, source, record_pages)
}

/// The keys of `db`'s `lib`, found from its source (`main` is not served, and `lib`'s def
/// map is resolved): no page of the record is read for them. The head is read after.
fn asked<'db>(db: &'db Db, source: &Counting) -> Asked<'db> {
    let lib = named(db, "lib");
    let mut trait_ = None;
    let mut adt = None;
    for (_, module) in hir_def::nameres::crate_def_map(db, lib).modules(db) {
        for def in module.scope(db).declarations() {
            match def {
                ModuleDefId::TraitId(it)
                    if hir_def::signatures::TraitSignature::of(db, it).name.as_str() == "Show" =>
                {
                    trait_ = Some(it)
                }
                ModuleDefId::AdtId(it) if adt.is_none() => adt = Some(it),
                _ => {}
            }
        }
    }
    let asked = Asked {
        trait_: trait_.expect("`Show`"),
        self_ty: S::Adt(adt.expect("a struct").into()),
        name: plain("show"),
    };
    // The head, which every lookup needs once a crate.
    _ = super::counts(db, lib);
    source.take();
    asked
}

/// A lookup reads the pages its key is on: the same few whether the crate has 40 impls of a
/// trait or 4,000, for a row, a type's inherent impls and a name's items, and for a key the
/// crate has nothing for (a definite "none" from the slot).
///
/// Its controls are [`a_whole_impl_table_reads_more_pages_than_a_row`] and
/// [`the_oracle_catches_an_absent_row_found_by_a_scan`].
#[test]
fn an_impl_lookup_reads_a_bounded_number_of_pages() {
    let mut records = Vec::new();
    for count in [40, 4000] {
        let (db, source, record_pages) = sized_and_read(count);
        let lib = named(&db, "lib");
        within(&db, || {
            let Asked { trait_, self_ty, name } = asked(&db, &source);
            let counted = [count as u32, 1, count as u32, count as u32 + 1, count as u32];
            assert_eq!(super::counts(&db, lib), counted);
            let bounded = |what: &str| {
                let touched = source.take();
                assert!(
                    (1..=LOOKUP_PAGES).contains(&touched),
                    "{count} types, {what}: {touched} pages read"
                );
            };
            let key = SelfTyKey(self_ty);
            let row = TraitImpls::in_crate_for_trait_and_self_ty(&db, lib, trait_, key);
            assert_eq!(row.len(), 1, "{count}");
            bounded("a row");
            let absent = SelfTyKey(S::Tuple(9));
            assert!(TraitImpls::in_crate_for_trait_and_self_ty(&db, lib, trait_, absent).is_empty());
            bounded("a row the trait has none of");
            assert_eq!(InherentImpls::in_crate_for_self_ty(&db, lib, key).len(), 1);
            bounded("a type's inherent impls");
            assert!(InherentImpls::in_crate_for_self_ty(&db, lib, absent).is_empty());
            bounded("the inherent impls of a type with none");
            assert_eq!(in_crate_trait_value_items(&db, lib, name).len(), 1);
            bounded("a name's items");
            assert!(in_crate_trait_value_items(&db, lib, plain("no_such")).is_empty());
            bounded("a name no trait binds");
            assert!(TraitImpls::served_blanket(&db, lib, trait_).blanket_impls(trait_).is_empty());
            bounded("a trait's blanket impls");
            // A type's trait impls: its window and entry, then its one row (two pages more
            // where the row crosses one).
            let mut impls = 0;
            super::for_self_ty(&db, lib, &self_ty, |_| impls += 1);
            assert_eq!(impls, 1, "{count}");
            let touched = source.take();
            assert!(
                (1..=LOOKUP_PAGES + 2).contains(&touched),
                "{count} types, a type's trait impls: {touched} pages read"
            );
            super::for_self_ty(&db, lib, &S::Tuple(9), |it| panic!("{it:?}"));
            bounded("the trait impls of a type with none");
        });
        records.push(record_pages);
    }
    // The larger record is far over the bound, so the bound is the lookup's, not the record's.
    assert!(records[1] > 10 * LOOKUP_PAGES, "{records:?}");
    assert!(records[1] > 10 * records[0], "{records:?}");
}

/// The control: the cause removed. The same crate's whole table asked for, as before a row
/// had a slot, reads far more pages than the bound.
#[test]
fn a_whole_impl_table_reads_more_pages_than_a_row() {
    let (db, source, _) = sized_and_read(4000);
    let lib = named(&db, "lib");
    within(&db, || {
        _ = asked(&db, &source);
        _ = TraitImpls::for_crate(&db, lib);
        let touched = source.take();
        assert!(touched > 2 * LOOKUP_PAGES, "{touched} pages read");
    });
}

/// The control of the negative read: a reader that scans for a row it did not find (the
/// planted bug) reads far more pages than the bound for a row the trait has none of.
#[test]
fn the_oracle_catches_an_absent_row_found_by_a_scan() {
    let (db, source, _) = sized_and_read(4000);
    let lib = named(&db, "lib");
    within(&db, || {
        let Asked { trait_, .. } = asked(&db, &source);
        SCANS_FOR_ABSENT.set(true);
        let absent = SelfTyKey(S::Tuple(8));
        let none = TraitImpls::in_crate_for_trait_and_self_ty(&db, lib, trait_, absent).is_empty();
        SCANS_FOR_ABSENT.set(false);
        assert!(none);
        let touched = source.take();
        assert!(touched > 2 * LOOKUP_PAGES, "{touched} pages read by the scan");
    });
}
