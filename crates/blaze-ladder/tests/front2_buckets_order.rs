//! Lane BK: the impl index of a body job (front2 buckets.rs `Buckets` over records, the one
//! `blaze check` takes) answers each question the same whatever was asked before it, and looks
//! each (crate, bucket) up once.
//!
//! The cause it guards (2026-11-17, tempo's cold `blaze check` past its 900 s limit): every
//! `Buckets::admit_asked` that filed an impl sorted ALL the job's filed lists again (its impls'
//! crates, unread, bare, blanket, bounded, inherent "any", the negatives) so binary searches
//! could read them: N questions, N sorts of lists that grow with the job. The lists are hash
//! sets now and nothing filed is sorted; what a question reads in order (its candidates, its
//! read list) is made and sorted per question. The risk of that change is an answer that
//! depends on the order impls were filed in, so this test asks one set of questions three
//! ways, each in a fresh index (the way a body job starts):
//! - ALONE: each question in its own index (nothing filed before it);
//! - FORWARD: all in one index, in order;
//! - REVERSED: all in one index, in reverse order;
//!
//! and requires equal answers: each question's candidates (sorted), the trait's negative impls
//! (sorted), which candidates are unread, and the question's read list. Then the answers are
//! checked against what the Reference's rules say for this fixture, and the lookups counted.
//!
//! Its first run found an order dependence older than the sets: a trait's negative impls were
//! every one filed so far (`Buckets::negative_of`), so `Bee: Tr` asked after `A: Tr` held `a`'s
//! `impl !Tr for NotTr` and asked alone did not; they are narrowed by the question's home now.
//!
//! THE IMPL TABLES (engine.md E3, D131) are held to the same answers: each crate's impls
//! written once by `write_impl_tables` (equal bytes for its rows in either order), read through
//! blaze-impls' lookups behind the `BucketSource` seam, asked alone, forward and reversed. The
//! candidates and the unread among them equal the index's; a negative impl is found at its own
//! shape, so the tables' negatives are the index's that fit the question (the items records
//! file a trait's negatives with no shape); and one question reads
//! each (crate, bucket) once. The reach walk's survey questions (`Param`) and `{integer}` are
//! among those asked.
//!
//! Four crates on the records path (as tests/front2_prim_self_foreign_impl.rs builds them):
//! `core` (the sized family, `Tr`, `Marker`, `Show` and the blanket `impl<T: Marker> Show for
//! T`), `a` (`A: Tr + Marker`, a negative `impl !Tr for NotTr`, a refused header `impl Tr for
//! Missing` whose self path names nothing, and an unread one `impl gone::Gone for A` whose
//! trait path names nothing), `b` (`Bee: Tr + Marker`, an inherent `impl Bee`) and `app`.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};

use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_impls::{ImplRead, Probe};
use blaze_ladder::front2::buckets::{AdmitError, Ask, BucketRead, BucketSource, Buckets, Home, Homes, ImplQuestionError, ItemsIndex, TableSet, cell_read, cells_of, inherent_home, prepare_home, trait_cell, trait_home, write_impl_tables};
use blaze_ladder::front2::tables::recorder::{ReadSink, TableRead};
use blaze_ladder::front2::tables::view::{Tables, TablesOpts};
use blaze_surface::BucketKey;
use blaze_ladder::front2::heads::Store;
use blaze_ladder::front2::keys::{Keys, checker_of};
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, NamesSource, Records};
use blaze_own::Alloc;
use blaze_scope::graph::{CrateId, Sysroot};
use blaze_scope::names::Written;
use blaze_scope::resolve::{CrateInput, Names, SourceTexts, aligned, resolve_crates};
use blaze_scope::task::Serial;
use blaze_scope::{items_record, names_record};
use blaze_surface::{Edition, Namespace, PathKind};
use blaze_typeck::{Shape, UintTy};
use blaze_unit_graph::{Implicit, Prelude, StdIs};

const CORE: &str = "#![no_core]
#![feature(no_core, lang_items)]
pub mod marker {
    #[lang = \"pointee_sized\"]
    pub trait PointeeSized {}
    #[lang = \"meta_sized\"]
    pub trait MetaSized: PointeeSized {}
    #[lang = \"sized\"]
    pub trait Sized: MetaSized {}
}
pub trait Tr {}
pub trait Marker {}
pub trait Show {}
impl<T: Marker> Show for T {}
pub mod prelude {
    pub mod v1 {
        pub use crate::marker::Sized;
    }
    pub mod rust_2015 {
        pub use super::v1::*;
    }
    pub mod rust_2018 {
        pub use super::v1::*;
    }
    pub mod rust_2021 {
        pub use super::v1::*;
    }
    pub mod rust_2024 {
        pub use super::v1::*;
    }
}
";

const A: &str = "#![no_std]
#![feature(negative_impls)]
use core::{Marker, Show, Tr};
pub struct A;
pub struct NotTr;
impl Tr for A {}
impl Marker for A {}
impl !Tr for NotTr {}
impl Tr for Missing {}
impl gone::Gone for A {}
";

const B: &str = "#![no_std]
use core::{Marker, Tr};
pub struct Bee;
impl Tr for Bee {}
impl Marker for Bee {}
impl Bee {
    pub fn go(&self) {}
}
";

const APP: &str = "#![no_std]
use core::Tr;
pub struct Own;
impl Tr for Own {}
";

/// World indexes: 0 `app` (the owner), 1 `core`, 2 `a`, 3 `b`.
const APP_AT: usize = 0;
const CORE_AT: usize = 1;
const A_AT: usize = 2;
const B_AT: usize = 3;

struct Texts;

impl SourceTexts<'static> for Texts {
    fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
        match (krate, path) {
            (APP_AT, "/app/lib.rs") => Some(APP),
            (CORE_AT, "/core/lib.rs") => Some(CORE),
            (A_AT, "/a/lib.rs") => Some(A),
            (B_AT, "/b/lib.rs") => Some(B),
            _ => None,
        }
    }
}

fn a() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn input(name: &'static str, root: &'static str, deps: &'static [(&'static str, usize)]) -> CrateInput<'static> {
    CrateInput {
        name,
        key: CrateItemKey(ContentId::of("blaze-ladder buckets order crate", name.as_bytes())),
        edition: Edition::E2021,
        implicit: Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: false, entry: false },
        root,
        shown_from: "/",
        features: &[],
        deps,
        cfg: &[],
        script: None,
    }
}

fn names() -> &'static Names<'static> {
    let crates = [
        input("app", "/app/lib.rs", &[("core", CORE_AT), ("a", A_AT), ("b", B_AT)]),
        input("core", "/core/lib.rs", &[]),
        input("a", "/a/lib.rs", &[("core", CORE_AT)]),
        input("b", "/b/lib.rs", &[("core", CORE_AT)]),
    ];
    let sysroot = Sysroot { core: Some(CrateId(CORE_AT as u32)), alloc: None, std: None, bstd: None };
    let names = resolve_crates(&crates, sysroot, &Texts, &Serial, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    Box::leak(Box::new(names))
}

struct Kept {
    names: Vec<&'static [u8]>,
    items: Vec<&'static [u8]>,
    files: Vec<HashMap<String, &'static str>>,
}

impl Load<'static> for Kept {
    fn names(&self, krate: usize) -> Option<&'static [u8]> {
        self.names.get(krate).copied()
    }

    fn items(&self, krate: usize) -> Option<&'static [u8]> {
        self.items.get(krate).copied()
    }

    fn file(&self, krate: usize, path: blaze_scope::package_path::PackagePath<'_>) -> Option<&'static str> {
        self.files.get(krate)?.get(path.as_str()).copied()
    }

    fn expansion(&self, _krate: usize, _call: ItemKey) -> Option<LoadedExpansion<'static>> {
        None
    }
}

/// The crates' records in world order, as `blaze check`'s body job gives them.
fn records() -> &'static Records<'static> {
    let names = names();
    let g = a();
    let mut kept = Kept { names: Vec::new(), items: Vec::new(), files: Vec::new() };
    let mut crate_names: Vec<&'static str> = Vec::new();
    let mut keys: Vec<CrateItemKey> = Vec::new();
    for c in names.crates().iter() {
        kept.names.push(aligned(&names_record::encode(c, g).unwrap_or_else(|e| panic!("the names record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
        kept.items.push(aligned(&items_record::encode(c, g).unwrap_or_else(|e| panic!("the items record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
        kept.files.push(c.files.iter().map(|f| (f.package.as_str().to_string(), f.text)).collect());
        crate_names.push(c.name);
        keys.push(c.key);
    }
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the records reader's frame")));
    let log: &'static Log<'static> = Box::leak(Box::new(Log::new(Vec::leak(crate_names), g)));
    let load: &'static Kept = Box::leak(Box::new(kept));
    Box::leak(Box::new(Records::new(Vec::leak(keys), load, log, frame, g)))
}

/// The impl of crate `krate` whose self path is `self_path` and whose trait path is `trait_path`
/// (`None`: an inherent impl), by its row.
fn impl_key(records: &'static Records<'static>, krate: usize, self_path: &str, trait_path: Option<&str>) -> (ItemKey, ItemKey) {
    let mut found = None;
    NamesSource::each_impl_row(records, krate, &mut |r| {
        if r.self_path == Some(self_path) && r.trait_path == trait_path {
            found = Some((r.key, r.module));
        }
    });
    found.unwrap_or_else(|| panic!("no impl of `{self_path}` by `{trait_path:?}` in crate {krate}"))
}

/// The item `name` names in `module`, by the walk every later stage uses.
fn walk(records: &'static Records<'static>, module: ItemKey, name: &str) -> ItemKey {
    let segments = [name];
    match NamesSource::walk(records, module, Namespace::Types, Written { kind: PathKind::Plain, segments: &segments[..] }) {
        Ok(r) if r.consumed == 1 => r.def.key,
        other => panic!("`{name}` did not walk to one item: {other:?}"),
    }
}

/// What one question answered, in a form two indexes are compared by.
#[derive(Debug, PartialEq, Eq)]
struct Answer {
    candidates: Vec<ItemKey>,
    negatives: Vec<ItemKey>,
    unread: Vec<ItemKey>,
    reads: Vec<BucketRead>,
}

/// Asks `ask` of `b` as the check does (check.rs `make_once`: the home from the coherence
/// rules, the impls filed from `source`, the candidates) and reads what it answered.
fn ask_of<B: BucketSource>(source: &B, records: &'static Records<'static>, keys: &mut Keys, store: &mut Store<'static>, b: &mut Buckets, ask: Ask) -> Answer {
    prepare_home(records, keys, store, ask).unwrap_or_else(|e| panic!("{ask:?}: prepare coherence: {e}"));
    let home = match ask {
        Ask::Trait(t, shape) => trait_home(records, keys, store, t, shape).unwrap_or_else(|e| panic!("{ask:?}: trait home: {e}")),
        Ask::Inherent(shape) => inherent_home(records, keys, store, shape).unwrap_or_else(|e| panic!("{ask:?}: inherent home: {e}")),
    };
    source.admit(b, records, keys, home, ask, store.tys.alloc()).unwrap_or_else(|e| panic!("{ask:?}: {e}"));
    let (mut candidates, mut negatives, reads) = match ask {
        Ask::Trait(t, shape) => (b.trait_candidates(t, shape, home).to_vec(), b.negative_of(t, shape, home).to_vec(), b.trait_reads(t, shape).to_vec()),
        Ask::Inherent(shape) => (b.inherent_candidates(shape, home).to_vec(), Vec::new(), b.inherent_reads(shape).to_vec()),
    };
    candidates.sort_unstable();
    negatives.sort_unstable();
    let unread = candidates.iter().copied().filter(|k| b.is_unread(*k)).collect();
    Answer { candidates, negatives, unread, reads }
}

/// [`ask_of`] over today's index (the items records' bucket section).
fn ask(records: &'static Records<'static>, keys: &mut Keys, store: &mut Store<'static>, b: &mut Buckets, q: Ask) -> Answer {
    ask_of(&ItemsIndex, records, keys, store, b, q)
}

/// The row the impl tables hold for `impl_`, from any crate's buckets.
fn table_row(tables: &TableSet<'_>, crates: &[CrateItemKey], impl_: ItemKey) -> Option<blaze_surface::ImplRow> {
    for c in crates {
        let Some(t) = tables.of(*c) else { continue };
        for bucket in t.records.buckets.iter() {
            let view = blaze_surface::BucketView::read(&bucket.bytes).unwrap_or_else(|e| panic!("a bucket of {c:x}: {e:?}"));
            if let Some(row) = view.find(impl_) {
                return Some(row.unwrap_or_else(|e| panic!("the row of {impl_:x}: {e:?}")));
            }
        }
    }
    None
}

/// Whether a row at `shape` can answer a question at the checker's `asked` (a blanket fits
/// every shape; the fixture's rows are at its types and `u8`).
fn row_fits(shape: blaze_surface::SelfShape, asked: Shape) -> bool {
    match (shape.shape(), asked) {
        (blaze_surface::Shape::Blanket, _) => true,
        (blaze_surface::Shape::Def(k), Shape::Adt(a)) => checker_of(k) == a,
        (blaze_surface::Shape::Uint(blaze_surface::UintWidth::U8), Shape::Uint(UintTy::U8) | Shape::AnyInt) => true,
        _ => false,
    }
}

fn fixture_tables(records: &'static Records<'static>, crates: &[usize]) -> &'static TableSet<'static> {
    let mut tables = TableSet::new(a());
    for &krate in crates {
        let mut rows = Vec::new();
        assert!(NamesSource::each_impl_row(records, krate, &mut |r| rows.push(r)), "crate {krate} has impl rows");
        tables.put(write_impl_tables(records, krate, &rows, a()).expect("fixture impl tables"));
    }
    Box::leak(Box::new(tables))
}

/// Keep every offer to the recorder so a duplicate cannot be hidden by its own deduplication.
#[derive(Default)]
struct ImplOffers(RefCell<Vec<TableRead>>);

impl ReadSink for ImplOffers {
    fn read(&self, read: TableRead) {
        if matches!(read, TableRead::Impls { .. }) {
            self.0.borrow_mut().push(read);
        }
    }
}

#[test]
fn ten_thousand_negative_goals_offer_each_dependency_once_and_derive_one_probe_id() {
    let records = records();
    let tables = fixture_tables(records, &[CORE_AT]);
    let root = NamesSource::root_module(records, CORE_AT).expect("core's root");
    let lib = walk(records, root, "Tr");
    let tr = checker_of(lib);
    let keys = Keys::new();
    let in_core = Home::In(CORE_AT, None);
    let home = Homes { all: in_core, bare: in_core, blanket: in_core, ..Homes::ANY };
    let core = NamesSource::crate_key(records, CORE_AT).expect("core's key");
    let absent = cells_of(&ImplRead::Filter { krate: core, probe: Probe::Trait(lib), may_hold: false }).expect("the absent trait has a cell");
    let unread = cells_of(&ImplRead::Filter { krate: core, probe: Probe::UnreadTrait, may_hold: false }).expect("the crate's unread-impl flag has a cell");
    assert_eq!(tables.probe_ids_derived(), 0);
    for _body in 0..2 {
        let sink = ImplOffers::default();
        let view = Tables::new(TablesOpts { records, impls: Some(tables), lang_crates: &[CORE_AT], reads: &sink });
        let buckets = Buckets::lazy(view);
        for _ in 0..10_000 {
            assert_eq!(buckets.admit_shared(view, &keys, home, Ask::Trait(tr, Shape::Bool), a()), Ok(0));
        }
        // These distinct questions pass the per-question cache and consult the same
        // negative trait probe and unread-impl flag. Neither needs a second offer.
        for shape in [Shape::Char, Shape::Uint(UintTy::U8), Shape::Tuple(2), Shape::AnyInt] {
            assert_eq!(buckets.admit_shared(view, &keys, home, Ask::Trait(tr, shape), a()), Ok(0));
        }
        assert_ne!(absent, unread);
        assert_eq!(sink.0.borrow().as_slice(), &[absent, unread]);
        assert_eq!(tables.probe_ids_derived(), 1);
    }
    println!("10,000 repeated goals per body: one named trait read and one unread-impl flag per body, one probe ID derived across two bodies");
}

/// A question whose trait or self type has no library key is ITS OWN refusal
/// (`AdmitError::Question`, which check.rs makes `CheckFail::ImplQuestion`: that body's NOT
/// RUN), never the tables' fault, and the next question over the same index still answers. The
/// twin: a crate whose tables were not written is the tables' fault (`AdmitError::Tables`,
/// `CheckFail::Tables`: a run's stop), as before. On both paths: the impl tables and the items
/// records' bucket index (`ItemsIndex`).
///
/// CONTROL (rule 23): before the split the refusal was `TablesReadError::NoLibraryKey`, under
/// `CheckFail::Tables`, which blaze-ladder scope.rs `CheckFail::scope` gives the suite's
/// stop; before D193 point 5's lead it was a names-log gap and `Ok(0)`, an answer of "no
/// impl". The first assertion fails on either.
#[test]
fn a_question_with_no_library_key_is_its_own_refusal_and_the_next_still_answers() {
    let records = records();
    let root = NamesSource::root_module(records, CORE_AT).expect("core's root");
    let tr = checker_of(walk(records, root, "Tr"));
    let root_a = NamesSource::root_module(records, A_AT).expect("a's root");
    let type_a = checker_of(walk(records, root_a, "A"));
    let (a_tr, _) = impl_key(records, A_AT, "A", Some("Tr"));
    // A checker key no row of the key map holds and no crate's records begin (the fixture's
    // crate keys are content digests, none all ones).
    let nowhere = blaze_typeck::ItemKey(u128::MAX);
    let keys = Keys::new();
    let no_key = |got: &Result<usize, AdmitError>| matches!(got, Err(AdmitError::Question(ImplQuestionError::NoLibraryKey { key, .. })) if *key == nowhere);

    // The impl tables.
    let crates = [APP_AT, CORE_AT, A_AT, B_AT];
    let tables = fixture_tables(records, &crates);
    let sink = ImplOffers::default();
    let view = Tables::new(TablesOpts { records, impls: Some(tables), lang_crates: &[CORE_AT], reads: &sink });
    let buckets = Buckets::lazy(view);
    let by_type = buckets.admit_shared(view, &keys, Homes::ANY, Ask::Trait(tr, Shape::Adt(nowhere)), a());
    assert!(no_key(&by_type), "tables: a self type with no library key is the question's refusal: {by_type:?}");
    let by_trait = buckets.admit_shared(view, &keys, Homes::ANY, Ask::Trait(nowhere, Shape::Adt(type_a)), a());
    assert!(no_key(&by_trait), "tables: a trait with no library key is the question's refusal: {by_trait:?}");
    buckets.admit_shared(view, &keys, Homes::ANY, Ask::Trait(tr, Shape::Adt(type_a)), a()).unwrap_or_else(|e| panic!("tables: `A: Tr` after the refused questions: {e}"));
    let candidates = buckets.trait_candidates(tr, Shape::Adt(type_a), Homes::ANY);
    assert!(candidates.contains(&a_tr), "tables: `A: Tr` still answers `impl Tr for A`: {candidates:?}");

    // The items records' bucket index.
    let mut index = Buckets::lazy(records);
    let by_type = index.admit_asked(records, &keys, Homes::ANY, Ask::Trait(tr, Shape::Adt(nowhere)));
    assert!(no_key(&by_type), "bucket index: a self type with no library key is the question's refusal: {by_type:?}");
    index.admit(records, Homes::ANY);
    index.admit_asked(records, &keys, Homes::ANY, Ask::Trait(tr, Shape::Adt(type_a))).unwrap_or_else(|e| panic!("bucket index: `A: Tr` after the refused question: {e}"));
    let candidates = index.trait_candidates(tr, Shape::Adt(type_a), Homes::ANY);
    assert!(candidates.contains(&a_tr), "bucket index: `A: Tr` still answers `impl Tr for A`: {candidates:?}");

    // TWIN: crate `a`'s tables not written. `A: Tr` at any home asks them: the tables' fault.
    let without_a = fixture_tables(records, &[APP_AT, CORE_AT, B_AT]);
    let sink = ImplOffers::default();
    let view = Tables::new(TablesOpts { records, impls: Some(without_a), lang_crates: &[CORE_AT], reads: &sink });
    let buckets = Buckets::lazy(view);
    let unwritten = buckets.admit_shared(view, &keys, Homes::ANY, Ask::Trait(tr, Shape::Adt(type_a)), a());
    assert!(matches!(unwritten, Err(AdmitError::Tables(_))), "a crate whose tables were not written is the tables' fault: {unwritten:?}");
    println!("no library key: the question's refusal on both paths, the next question answers; tables not written: {}", unwritten.err().map(|e| e.to_string()).unwrap_or_default());
}

#[test]
fn a_bodys_distinct_impl_reads_equal_the_physical_lookup_read_set() {
    use blaze_surface::{SelfShape, Shape as SurfaceShape};

    let records = records();
    let crates = [APP_AT, CORE_AT, A_AT, B_AT];
    let crate_keys = crates.map(|krate| NamesSource::crate_key(records, krate).expect("fixture crate"));
    let tables = fixture_tables(records, &crates);
    let root = NamesSource::root_module(records, CORE_AT).expect("core's root");
    let tr = walk(records, root, "Tr");
    let show = walk(records, root, "Show");
    let root_a = NamesSource::root_module(records, A_AT).expect("a's root");
    let root_b = NamesSource::root_module(records, B_AT).expect("b's root");
    let type_a = walk(records, root_a, "A");
    let bee = walk(records, root_b, "Bee");
    let goals = [(Some(tr), type_a), (Some(tr), bee), (Some(show), bee), (None, bee), (Some(tr), type_a)];

    // The old recorder took every physical note with a cell. Distinctness here is
    // by the complete writer-visible cell, independent of any dense ID or stamp.
    for (trait_, ty) in goals {
        let shape = SelfShape::new(SurfaceShape::Def(ty)).expect("fixture shape");
        let found = match trait_ {
            Some(t) => blaze_impls::candidates_in(&crate_keys, t, shape, tables),
            None => blaze_impls::inherent_candidates_in(&crate_keys, shape, tables),
        };
        for row in found {
            row.expect("the physical lookup reads a fixture impl row");
        }
    }
    // The ungrouped oracle reads blanket and blind buckets separately for every width.
    // Show has a blanket impl, while several of these crates can reject Show outright.
    for width in [blaze_surface::FloatWidth::F16, blaze_surface::FloatWidth::F32, blaze_surface::FloatWidth::F64, blaze_surface::FloatWidth::F128] {
        let shape = SelfShape::new(SurfaceShape::Float(width)).expect("a concrete float width");
        for row in blaze_impls::candidates_in(&crate_keys, show, shape, tables) {
            row.expect("float lookup");
        }
    }
    let notes = tables.take_notes();
    assert!(notes.iter().any(|r| matches!(r, ImplRead::Bucket { .. })), "exercise a present bucket");
    assert!(notes.iter().any(|r| matches!(r, ImplRead::Filter { may_hold: false, .. })), "exercise an absent bucket");
    let expected: BTreeSet<_> = notes.iter().filter_map(cells_of).collect();
    assert!(expected.len() > 1 && notes.iter().filter_map(cells_of).count() > expected.len(), "fixture has distinct and repeated cells");

    let keys = Keys::new();
    let sink = ImplOffers::default();
    let view = Tables::new(TablesOpts { records, impls: Some(tables), lang_crates: &[CORE_AT], reads: &sink });
    let buckets = Buckets::lazy(view);
    for (trait_, ty) in goals {
        let shape = Shape::Adt(checker_of(ty));
        let ask = trait_.map_or(Ask::Inherent(shape), |t| Ask::Trait(checker_of(t), shape));
        buckets.admit_shared(view, &keys, Homes::ANY, ask, a()).expect("the body's fixture lookup");
    }
    buckets.admit_shared(view, &keys, Homes::ANY, Ask::Trait(checker_of(show), Shape::AnyFloat), a()).expect("the numeric survey");
    let offers = sink.0.borrow();
    assert_eq!(offers.iter().copied().collect::<BTreeSet<_>>(), expected);
    assert_eq!(offers.len(), expected.len(), "each distinct bucket reaches the sink once per body");
    println!("mixed trait and inherent goals: each physical lookup dependency recorded once");
}

#[test]
fn impl_questions_answer_alike_in_any_order_and_look_each_bucket_up_once() {
    let records = records();
    // Keep the synthetic store frame alive for the `'static` records used by this test.
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the store's frame")));
    let mut store: Store<'static> = Store::new(None, frame, a());
    let mut keys = Keys::new();

    // The impls, by their rows; the traits and types, walked in the impl's module.
    let (a_tr, a_mod) = impl_key(records, A_AT, "A", Some("Tr"));
    let (missing_tr, _) = impl_key(records, A_AT, "Missing", Some("Tr"));
    let (gone_a, _) = impl_key(records, A_AT, "A", Some("gone::Gone"));
    let (not_tr, _) = impl_key(records, A_AT, "NotTr", Some("Tr"));
    let (bee_tr, b_mod) = impl_key(records, B_AT, "Bee", Some("Tr"));
    let (bee_inherent, _) = impl_key(records, B_AT, "Bee", None);
    let (own_tr, _) = impl_key(records, APP_AT, "Own", Some("Tr"));
    let mut blanket = None;
    NamesSource::each_impl_row(records, CORE_AT, &mut |r| {
        if r.trait_path == Some("Show") {
            blanket = Some(r.key);
        }
    });
    let blanket = blanket.expect("core's blanket `impl<T: Marker> Show for T`");
    let tr = checker_of(walk(records, a_mod, "Tr"));
    let show = checker_of(walk(records, a_mod, "Show"));
    let at_a = Shape::Adt(checker_of(walk(records, a_mod, "A")));
    let at_not_tr = Shape::Adt(checker_of(walk(records, a_mod, "NotTr")));
    let at_bee = Shape::Adt(checker_of(walk(records, b_mod, "Bee")));

    let asks = [
        Ask::Trait(tr, at_a),
        Ask::Trait(tr, at_bee),
        Ask::Trait(show, at_bee),
        Ask::Trait(show, at_a),
        Ask::Trait(tr, at_not_tr),
        Ask::Trait(tr, Shape::Uint(UintTy::U8)),
        Ask::Inherent(at_bee),
        Ask::Inherent(at_a),
        // The reach walk's survey asks each trait at `Param` first (reach/instances.rs
        // `every_met_shape`): the blanket and unread impls alone.
        Ask::Trait(show, Shape::Param),
        Ask::Trait(tr, Shape::Param),
        // `{integer}`: every integer type's buckets.
        Ask::Trait(tr, Shape::AnyInt),
    ];

    // ALONE: each question in a fresh index.
    let alone: Vec<Answer> = asks
        .iter()
        .map(|q| {
            let mut b = Buckets::lazy(records);
            ask(records, &mut keys, &mut store, &mut b, *q)
        })
        .collect();
    // FORWARD and REVERSED: all in one index each.
    let mut forward_index = Buckets::lazy(records);
    let forward: Vec<Answer> = asks.iter().map(|q| ask(records, &mut keys, &mut store, &mut forward_index, *q)).collect();
    let mut reversed_index = Buckets::lazy(records);
    let mut reversed: Vec<Answer> = asks.iter().rev().map(|q| ask(records, &mut keys, &mut store, &mut reversed_index, *q)).collect();
    reversed.reverse();
    for (i, q) in asks.iter().enumerate() {
        println!("ASK {q:?}: {} candidates, {} negatives, {} unread, {} reads", alone[i].candidates.len(), alone[i].negatives.len(), alone[i].unread.len(), alone[i].reads.len());
        assert_eq!(alone[i], forward[i], "{q:?}: asked alone and asked after the questions before it answer differently");
        assert_eq!(alone[i], reversed[i], "{q:?}: asked alone and asked after the questions after it answer differently");
    }

    // What the rules say for this fixture (the Reference, "Implementations", orphan rules).
    let has = |i: usize, k: ItemKey| alone[i].candidates.contains(&k);
    // `A: Tr`: its impl; the refused `impl Tr for Missing` (its self path names nothing, so by
    // the orphan rules it is bounded to `a`'s local types, which `A` is); the unread
    // `impl gone::Gone for A` (a trait impl at `A`'s shape whose trait was not read).
    assert!(has(0, a_tr), "`A: Tr` lacks `impl Tr for A`");
    assert!(has(0, missing_tr), "`A: Tr` lacks the refused `impl Tr for Missing`, a candidate at `a`'s types");
    assert!(has(0, gone_a) && alone[0].unread.contains(&gone_a), "`A: Tr` lacks the unread `impl gone::Gone for A`");
    // `Bee: Tr`: its impl, and neither of `a`'s refused or unread impls (another crate's type,
    // another shape), nor another type's impl.
    assert!(has(1, bee_tr), "`Bee: Tr` lacks `impl Tr for Bee`");
    for (k, what) in [(missing_tr, "impl Tr for Missing"), (gone_a, "impl gone::Gone for A"), (a_tr, "impl Tr for A"), (own_tr, "impl Tr for Own")] {
        assert!(!has(1, k), "`Bee: Tr` holds `{what}`");
    }
    // `Bee: Show` and `A: Show`: core's blanket impl.
    assert!(has(2, blanket) && has(3, blanket), "a `Show` question lacks core's blanket impl");
    // `NotTr: Tr`: the negative impl is the trait's negative, never a positive candidate.
    assert!(alone[4].negatives.contains(&not_tr), "`Tr`'s negatives lack `impl !Tr for NotTr`");
    assert!(!has(4, not_tr), "`NotTr: Tr` holds the negative impl as a candidate");
    // `u8: Tr`: no impl of `Tr` is written at `u8` (`Tr` has no parameter but `Self`, so only
    // core may write one, and core writes none): no type's impl is a candidate.
    for k in [a_tr, bee_tr, own_tr, missing_tr] {
        assert!(!has(5, k), "`u8: Tr` holds an impl of another type");
    }
    // `Bee`'s inherent question: `impl Bee`; `A`'s: not it.
    assert!(has(6, bee_inherent), "`Bee`'s inherent question lacks `impl Bee`");
    assert!(!has(7, bee_inherent), "`A`'s inherent question holds `impl Bee`");

    // THE COUNT: N questions do one lookup per (crate, bucket) they read, however many of the
    // questions read it, and asking them again looks nothing up. Each question's read list
    // names each (crate, bucket) it read once ([`BucketRead`]; the unfiled list and the
    // incoherent fact are read lists' rows too, but no bucket lookup).
    let mut distinct: Vec<(u128, u64)> = alone.iter().flat_map(|q| q.reads.iter().filter(|r| r.bucket > 2).map(|r| (r.krate, r.bucket))).collect();
    distinct.sort_unstable();
    distinct.dedup();
    println!("LOOKUPS: {} questions, {} lookups in one index, {} distinct (crate, bucket) read", forward_index.bucket_asks(), forward_index.bucket_lookups(), distinct.len());
    assert_eq!(forward_index.bucket_asks() as usize, asks.len(), "every question is asked by bucket");
    assert_eq!(forward_index.bucket_lookups() as usize, distinct.len(), "one index looks each (crate, bucket) up once");
    assert_eq!(reversed_index.bucket_lookups(), forward_index.bucket_lookups(), "the lookups do not depend on the order asked");
    let before = forward_index.bucket_lookups();
    for q in asks.iter() {
        ask(records, &mut keys, &mut store, &mut forward_index, *q);
    }
    assert_eq!(forward_index.bucket_lookups(), before, "a question asked again looks a bucket up again");

    // THE IMPL TABLES (engine.md E3): each crate's impls filed once into blaze-surface's
    // records by `write_impl_tables`, read through blaze-impls' lookups. Equal bytes for the
    // rows in either order; then every question answers as the index above answers it.
    let g = a();
    let mut tables = TableSet::new(g);
    let mut crate_keys: Vec<CrateItemKey> = Vec::new();
    for krate in [APP_AT, CORE_AT, A_AT, B_AT] {
        let mut rows = Vec::new();
        assert!(NamesSource::each_impl_row(records, krate, &mut |r| rows.push(r)), "crate {krate} has impl rows");
        let written = write_impl_tables(records, krate, &rows, g).unwrap_or_else(|e| panic!("crate {krate}'s impl tables: {e}"));
        assert!(written.records.refused.is_empty(), "crate {krate}'s impl tables refused rows: {:?}", written.records.refused);
        rows.reverse();
        let again = write_impl_tables(records, krate, &rows, g).unwrap_or_else(|e| panic!("crate {krate}'s impl tables, rows reversed: {e}"));
        assert_eq!(again.key, written.key, "crate {krate}: the tables' key depends on the rows' order");
        assert_eq!(again.records.directory, written.records.directory, "crate {krate}: the directory depends on the rows' order");
        assert_eq!(again.records.no_impls_of, written.records.no_impls_of, "crate {krate}: NoImplsOf depends on the rows' order");
        let bytes = |t: &blaze_ladder::front2::buckets::ImplTables<'_>| t.records.buckets.iter().map(|b| b.bytes.to_vec()).collect::<Vec<_>>();
        assert_eq!(bytes(&again), bytes(&written), "crate {krate}: the buckets depend on the rows' order");
        // Chunked filing (`file_impl_rows_chunk` + `finish_impl_tables`) must match single-pass byte-for-byte.
        let chunk_size = (rows.len() / 3).max(1);
        let mut chunks = blaze_own::Vec::new_in(g);
        for slice in rows.chunks(chunk_size) {
            chunks.push(blaze_ladder::front2::buckets::file_impl_rows_chunk(records, krate, slice, g).unwrap_or_else(|e| panic!("crate {krate} chunk: {e}")));
        }
        let chunked = blaze_ladder::front2::buckets::finish_impl_tables(written.krate, &chunks, g).unwrap_or_else(|e| panic!("crate {krate} finish: {e}"));
        assert_eq!(chunked.key, written.key, "crate {krate}: chunked key differs");
        assert_eq!(chunked.records.directory, written.records.directory, "crate {krate}: chunked directory differs");
        assert_eq!(chunked.records.no_impls_of, written.records.no_impls_of, "crate {krate}: chunked NoImplsOf differs");
        assert_eq!(bytes(&chunked), bytes(&written), "crate {krate}: chunked buckets differ");
        assert_eq!(chunked.defines.as_slice(), written.defines.as_slice(), "crate {krate}: chunked defines differ");
        assert_eq!(chunked.named.as_slice(), written.named.as_slice(), "crate {krate}: chunked named differs");
        println!("TABLES crate {krate}: {} rows, {} buckets, {} from rows, {} from text, {} named crates", rows.len(), written.records.buckets.len(), written.counts.from_rows, written.counts.from_text, written.named.len());
        crate_keys.push(written.krate);
        tables.put(written);
    }

    // THE CELLS: every read a lookup makes is a cell the tables define, or one they do not
    // (absent: what a body that found no impl read). A directory read that found a bucket saw
    // the cell's digest; one that found none, and a trait probe that said "none", saw it absent.
    let defined = |c: CrateItemKey, cell: [u8; 16]| tables.of(c).and_then(|t| t.defines.binary_search_by_key(&cell, |d| d.0).ok().map(|at| t.defines[at].1));
    let lib_tr = walk(records, a_mod, "Tr");
    for (what, shape) in [("A", blaze_surface::Shape::Def(walk(records, a_mod, "A"))), ("u8", blaze_surface::Shape::Uint(blaze_surface::UintWidth::U8)), ("any", blaze_surface::Shape::Placeholder)] {
        let shape = blaze_surface::SelfShape::new(shape).unwrap_or_else(|e| panic!("{what}: {e:?}"));
        tables.take_notes();
        let found = blaze_impls::candidates_in(&crate_keys, lib_tr, shape, &tables).count();
        let reads = tables.take_notes();
        assert!(!reads.is_empty(), "`{what}: Tr` read nothing");
        for read in reads.iter() {
            let krate = match read {
                ImplRead::Filter { krate, .. } | ImplRead::Directory { krate, .. } | ImplRead::Bucket { krate, .. } => *krate,
            };
            let Some(cell) = cell_read(read) else { continue };
            // The one map a body's recorder calls gives the same cell, of the same crate.
            let mut k16 = [0u8; 16];
            k16.copy_from_slice(&krate.0.0[..16]);
            assert_eq!(cells_of(read), Some(TableRead::Impls { krate: k16, bucket: cell }), "`{what}: Tr`: `cells_of` disagrees with `cell_read`");
            let saw = defined(krate, cell);
            match read {
                ImplRead::Directory { found, .. } => assert_eq!(saw, *found, "`{what}: Tr`: a directory read of {krate:x} is not its cell"),
                ImplRead::Filter { probe: Probe::Trait(_), may_hold: false, .. } => assert_eq!(saw, None, "`{what}: Tr`: {krate:x} said none of `Tr` but defines its cell"),
                ImplRead::Filter { probe: Probe::Trait(_), may_hold: true, .. } => {}
                ImplRead::Filter { probe: Probe::UnreadTrait, may_hold, .. } => assert_eq!(saw.is_some(), *may_hold, "the crate's unread-impl cell must agree with its filter"),
                ImplRead::Filter { probe: Probe::Bucket(_), may_hold, .. } => assert!(*may_hold || saw.is_none(), "`{what}: Tr`: {krate:x} said no bucket but defines its cell"),
                ImplRead::Bucket { .. } => {}
            }
        }
        println!("CELLS `{what}: Tr`: {found} rows, {} reads", reads.len());
    }
    assert!(crate_keys.iter().all(|c| defined(*c, trait_cell(lib_tr)).is_some() == tables.of(*c).is_some_and(|t| t.records.buckets.iter().any(|b| matches!(b.key, BucketKey::Trait { trait_, .. } | BucketKey::Blanket { trait_ } if trait_ == lib_tr)))), "a crate's `Tr` cell disagrees with its buckets");

    // ALONE, FORWARD and REVERSED over the tables.
    let tables_alone: Vec<(Answer, u32)> = asks
        .iter()
        .map(|q| {
            let mut b = Buckets::lazy(records);
            let answer = ask_of(&tables, records, &mut keys, &mut store, &mut b, *q);
            (answer, b.bucket_lookups())
        })
        .collect();
    let mut tables_forward_index = Buckets::lazy(records);
    let tables_forward: Vec<Answer> = asks.iter().map(|q| ask_of(&tables, records, &mut keys, &mut store, &mut tables_forward_index, *q)).collect();
    let mut tables_reversed_index = Buckets::lazy(records);
    let mut tables_reversed: Vec<Answer> = asks.iter().rev().map(|q| ask_of(&tables, records, &mut keys, &mut store, &mut tables_reversed_index, *q)).collect();
    tables_reversed.reverse();
    for (i, q) in asks.iter().enumerate() {
        let (t, lookups) = &tables_alone[i];
        println!("TABLES ASK {q:?}: {} candidates, {} negatives, {} unread, {} reads, {lookups} bucket reads", t.candidates.len(), t.negatives.len(), t.unread.len(), t.reads.len());
        assert_eq!(*t, tables_forward[i], "{q:?}: over the tables, asked alone and asked after the questions before it answer differently");
        assert_eq!(*t, tables_reversed[i], "{q:?}: over the tables, asked alone and asked after the questions after it answer differently");
        // EQUAL to the index: the candidates and which are unread.
        assert_eq!(t.candidates, alone[i].candidates, "{q:?}: the tables' candidates differ from the index's");
        assert_eq!(t.unread, alone[i].unread, "{q:?}: the tables' unread candidates differ from the index's");
        // The negatives: the index's that can answer the question. The items records' bucket
        // section files a trait's negatives with no shape (its `negatives` bucket), so the
        // index holds every one; the tables file each at its own shape.
        let shape = match q {
            Ask::Trait(_, s) | Ask::Inherent(s) => *s,
        };
        let fitting: Vec<ItemKey> = alone[i]
            .negatives
            .iter()
            .copied()
            .filter(|k| table_row(&tables, &crate_keys, *k).is_some_and(|row| row_fits(row.shape, shape)))
            .collect();
        assert_eq!(t.negatives, fitting, "{q:?}: the tables' negatives are not the index's at the question's shape");
        // ONE PROBE per (crate, bucket) a question reads: its bucket reads are distinct.
        let distinct = t.reads.iter().filter(|r| r.bucket == r.answer && r.answer > 1).count();
        assert_eq!(*lookups as usize, distinct, "{q:?}: a bucket was read twice for one question");
    }
    // `NotTr: Tr` finds `impl !Tr for NotTr` in the tables too; `A: Tr` does not.
    assert!(tables_alone[4].0.negatives.contains(&not_tr), "the tables lack `impl !Tr for NotTr` at `NotTr`");
    assert!(!tables_alone[0].0.negatives.contains(&not_tr), "the tables give `impl !Tr for NotTr` at `A`");
    let before = tables_forward_index.bucket_lookups();
    for q in asks.iter() {
        ask_of(&tables, records, &mut keys, &mut store, &mut tables_forward_index, *q);
    }
    assert_eq!(tables_forward_index.bucket_lookups(), before, "over the tables, a question asked again reads a bucket again");
    let errors: Vec<String> = records.errors().iter().map(|e| e.to_string()).collect();
    println!("RECORD ERRORS: {errors:?}");
}
