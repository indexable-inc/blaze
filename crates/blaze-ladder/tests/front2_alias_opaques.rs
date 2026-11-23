//! A dependent body sees the bounds of an opaque even when only a field type names it.
//! All sources and records live in the fixture. A fresh consumer gets the dependency's
//! published heads through the same frozen table that a body task reads.

#![feature(allocator_api)]

use blaze_drive::key::{ABSENT, Key, Kind, Name};
use blaze_engine::{Phase, Planned, Stage, Task};
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::head_record::{self, HeadAt, HeadRecordError, OpaqueOrigin};
use blaze_ladder::front2::heads::{NeedsMet, Store, Want};
use blaze_ladder::front2::keys::{checker_of, opaque_key};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::front2::project::head_digest::{opaque_origin, want_heads};
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, NamesSource, Records};
use blaze_ladder::front2::tables::head::{self, FreezeError, FrozenHead, Plant};
use blaze_ladder::front2::tables::recorder::{Recorder, RecorderOpts, TableRead};
use blaze_ladder::front2::tables::seeds::{FrozenTable, HeadSeed, SeedError};
use blaze_ladder::front2::tables::task::{BodyFrame, BodyReads, CellAsk, TaskError, check_body_reads};
use blaze_ladder::front2::tables::view::{Tables, TablesOpts};
use blaze_ladder::refusal::SourceFile;
use blaze_mem::Frame;
use blaze_own::Alloc;
use blaze_scope::graph::{CrateId, Sysroot};
use blaze_scope::package_path::PackagePath;
use blaze_scope::resolve::{CrateInput, Names, SourceTexts, aligned, resolve_crates};
use blaze_scope::task::Serial;
use blaze_scope::{items_record, names_record};
use blaze_surface::{Edition, ItemKind};
use blaze_unit_graph::{Implicit, Prelude, StdIs};

const USER: &str = r#"#![no_core]
#![feature(no_core)]
extern crate dep;
extern crate core;
fn require_send<T: core::marker::Send>() {}
pub fn direct() { require_send::<dep::Holder>(); }
pub fn nested() { require_send::<dep::LazyHolder>(); }
pub fn scalar() -> u8 { 7 }
"#;

const CORE: &str = r#"#![no_core]
#![feature(no_core, lang_items, auto_traits, unboxed_closures, rustc_attrs)]
#[lang = "pointee_sized"] pub trait PointeeSized {}
#[lang = "meta_sized"] pub trait MetaSized: PointeeSized {}
#[lang = "sized"] pub trait Sized: MetaSized {}
#[lang = "copy"] pub trait Copy {}
#[lang = "drop"] pub trait Drop { fn drop(&mut self); }
#[lang = "tuple_trait"] pub trait Tuple {}
pub mod ops {
    use crate::Tuple;
    #[lang = "fn_once"] #[rustc_paren_sugar]
    pub trait FnOnce<A: Tuple> {
        #[lang = "fn_once_output"] type Output;
        extern "rust-call" fn call_once(self, args: A) -> Self::Output;
    }
    #[lang = "fn_mut"] #[rustc_paren_sugar]
    pub trait FnMut<A: Tuple>: FnOnce<A> {
        extern "rust-call" fn call_mut(&mut self, args: A) -> Self::Output;
    }
    #[lang = "fn"] #[rustc_paren_sugar]
    pub trait Fn<A: Tuple>: FnMut<A> {
        extern "rust-call" fn call(&self, args: A) -> Self::Output;
    }
}
pub mod marker { pub unsafe auto trait Send {} }
"#;

const DEP: &str = r#"#![no_core]
#![feature(no_core, type_alias_impl_trait)]
extern crate core;
use core::ops::Fn;
use core::marker::Send;
pub type Hidden = impl Fn() -> u8 + Send;
#[define_opaque(Hidden)] pub fn hidden() -> Hidden { || 7u8 }
pub struct Holder { pub h: Hidden }
pub struct LazyLock<T, F> { value: T, init: F }
pub struct LazyHolder { pub lock: LazyLock<u8, Hidden> }
"#;

const FILES: [(&str, &str); 3] = [("/user/src/lib.rs", USER), ("/dep/src/lib.rs", DEP), ("/core/src/lib.rs", CORE)];
const LANG: &[usize] = &[0, 1, 2];

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

struct Texts { dep: &'static str }

impl SourceTexts<'static> for Texts {
    fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
        let &(root, text) = FILES.get(krate)?;
        (root == path).then_some(if krate == 1 { self.dep } else { text })
    }
}

struct Kept {
    names: Vec<&'static [u8]>,
    items: Vec<&'static [u8]>,
    dep: &'static str,
}

impl Load<'static> for Kept {
    fn names(&self, krate: usize) -> Option<&'static [u8]> { self.names.get(krate).copied() }
    fn items(&self, krate: usize) -> Option<&'static [u8]> { self.items.get(krate).copied() }
    fn file(&self, krate: usize, path: PackagePath<'_>) -> Option<&'static str> {
        let &(root, text) = FILES.get(krate)?;
        (root.strip_prefix('/') == Some(path.as_str())).then_some(if krate == 1 { self.dep } else { text })
    }
    fn expansion(&self, _: usize, _: ItemKey) -> Option<LoadedExpansion<'static>> { None }
}

struct Fixture {
    names: &'static Names<'static>,
    records: &'static Records<'static>,
    frame: &'static Frame,
    heads: Vec<(Want, Vec<u8>)>,
}

impl Fixture {
    fn solve(dep: &'static str) -> Self {
        let input = |name: &'static str, root: &'static str, deps: &'static [(&'static str, usize)]| CrateInput {
            name, root, shown_from: "/", deps, features: &[], cfg: &[], script: None,
            key: CrateItemKey(ContentId::of("front2 alias opaque fixture", name.as_bytes())),
            edition: Edition::E2021,
            implicit: Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: false, entry: false },
        };
        let crates = [
            input("user", FILES[0].0, &[("dep", 1), ("core", 2)]),
            input("dep", FILES[1].0, &[("core", 2)]),
            input("core", FILES[2].0, &[]),
        ];
        let sysroot = Sysroot { core: Some(CrateId(2)), alloc: None, std: None, bstd: None };
        let names: &'static Names<'static> = Box::leak(Box::new(resolve_crates(&crates, sysroot, &Texts { dep }, &Serial, blaze_own::testing::bump()).expect("fixture names")));
        let frame: &'static Frame = Box::leak(Box::new(Frame::new(blaze_mem::tag::Tag::Infer).expect("fixture frame")));
        let mut kept = Kept { names: Vec::new(), items: Vec::new(), dep };
        let mut crate_keys = Vec::new();
        for c in names.crates() {
            crate_keys.push(c.key);
            kept.names.push(aligned(&names_record::encode(c, alloc()).expect("names record"), blaze_own::testing::bump()));
            kept.items.push(aligned(&items_record::encode(c, alloc()).expect("items record"), blaze_own::testing::bump()));
        }
        let log = Box::leak(Box::new(Log::new(&["user", "dep", "core"], alloc())));
        let records = Box::leak(Box::new(Records::new(crate_keys.leak(), Box::leak(Box::new(kept)), log, frame, alloc())));
        Fixture { names, frame, records, heads: Vec::new() }
    }

    fn producer(&self) -> (Front2<'static>, Store<'static>) {
        let user = [SourceFile { path: "user/src/lib.rs", text: USER }];
        let front = Front2::new(self.names, self.frame, &user, alloc()).expect("solved front end");
        let store = Store::new(front.index.lang("sized"), self.frame, alloc());
        (front, store)
    }

    fn new() -> Self {
        let mut fixture = Self::solve(DEP);
        let names = fixture.names;
        let (mut front, mut store) = fixture.producer();
        for krate in LANG {
            names.each_item(*krate, &mut |lib, _| {
                let want = match names.kind_of_item(lib) {
                    Some(ItemKind::Struct) => Want::Adt(lib),
                    Some(ItemKind::TypeAlias) => Want::Head(lib),
                    Some(ItemKind::Function) => Want::Fn(lib),
                    Some(ItemKind::Trait) => Want::Members(lib),
                    _ => return,
                };
                store.ensure(names, &mut front.keys, want).unwrap_or_else(|e| panic!("making {want:?}: {e}"));
                let bytes = head::export(names, &front.keys, &mut store, want, Plant::None, alloc()).unwrap_or_else(|e| panic!("freezing {want:?}: {e}"));
                fixture.heads.push((want, bytes.to_vec()));
            });
        }
        fixture
    }

    fn key(&self, krate: usize, name: &str) -> ItemKey {
        let mut found = None;
        self.names.each_item(krate, &mut |key, _| {
            if self.names.name_of(key) == Some(name) {
                assert!(found.replace(key).is_none(), "ambiguous fixture item {name}");
            }
        });
        found.unwrap_or_else(|| panic!("missing fixture item {name}"))
    }

    fn bytes(&mut self, want: Want) -> &mut Vec<u8> {
        &mut self.heads.iter_mut().find(|(w, _)| *w == want).expect("published cell").1
    }

    fn table(&self) -> FrozenTable<'static> {
        FrozenTable::from_frozen(self.heads.iter().map(|(_, bytes)| bytes.as_slice()), alloc()).expect("open published heads")
    }

    fn task(&self, name: &str) -> Result<BodyReads<'static>, TaskError> {
        let recorder = Recorder::new(RecorderOpts { alloc: alloc() });
        let table = self.frame.alloc(self.table());
        let seed = self.frame.alloc(table.view(self.records));
        let tables = Tables::new(TablesOpts { records: self.records, impls: None, lang_crates: LANG, reads: &recorder });
        let frame = BodyFrame { frame: self.frame, alloc: alloc(), needs: NeedsMet::All, heads: Some(seed), cost: None, plant_drop: None, sources: None, goals: None, prepared: None };
        let task = Task { planned: Planned { name: Name::new(Kind::BodyCheck, self.key(0, name).0), phase: Phase::new(Stage::Bodies, 0), source: ABSENT, file: Name::new(Kind::FileItems, ABSENT) }, key: Key(ABSENT) };
        check_body_reads(tables, &task, &frame, alloc())
    }

    fn clean(&self, name: &str) -> BodyReads<'static> {
        let result = self.task(name).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(!result.failed && result.findings == 0 && result.said.is_empty(), "{name}: {:?}", result.said);
        assert!(self.records.errors().is_empty(), "unexpected record errors: {:?}", self.records.errors());
        result
    }
}

fn clean_in_both(name: &str) {
    let fixture = Fixture::new();
    let user = [SourceFile { path: "user/src/lib.rs", text: USER }];
    let mut solved = ProjectWorld::new(fixture.names, fixture.frame, &user, alloc()).expect("solved world");
    let verdict = solved.check(BodyRef::Free(fixture.key(0, name)), alloc());
    assert!(verdict.is_clean() && verdict.undecided.is_empty() && verdict.reads.is_some(), "{name}: {verdict:?}");
    let checked = fixture.clean(name);
    let alias = checker_of(fixture.key(1, "Hidden")).0.to_be_bytes();
    assert!(checked.reads.contains(&TableRead::Head { item: alias, part: 0 }), "{name} must read the defining head");
    println!("{name}: clean over solved names and reopened records; read dep::Hidden");
}

#[test]
fn dependency_field_retains_its_opaque_owner() {
    clean_in_both("direct");
}

#[test]
fn lazy_lock_argument_retains_its_opaque_owner() {
    clean_in_both("nested");
}

#[test]
fn current_terms_name_the_alias_and_round_trip_in_a_fresh_type_table() {
    let fixture = Fixture::solve(DEP);
    let alias = fixture.key(1, "Hidden");
    let holder = fixture.key(1, "Holder");
    let owner = checker_of(alias);
    let opaque = opaque_key(owner, 0);
    let expected = OpaqueOrigin { want: Want::Head(alias), owner, index: 1u32 << 31 };
    let (mut front, mut producer) = fixture.producer();
    producer.ensure(fixture.names, &mut front.keys, Want::Adt(holder)).expect("lower Holder and its field type");
    let alias_rows = want_heads(&producer, expected.want, alloc());
    assert_eq!(&alias_rows[..], &[HeadAt::Alias(alias), HeadAt::Opaque(opaque)]);
    for at in [HeadAt::Adt(holder), HeadAt::Alias(alias), HeadAt::Opaque(opaque)] {
        let head = head_record::from_store(&producer, at).expect("producer head");
        let bytes = head_record::encode_with_origins(head, producer.tys.view(), alloc(), &|key| {
            opaque_origin(fixture.names, &front.keys, &producer, key)
        }).expect("current term and defining reference");
        assert_eq!(&bytes[4..8], &head_record::VERSION.to_le_bytes());
        let fresh = Store::new(None, fixture.frame, alloc());
        let mut references = Vec::new();
        head_record::decode_with_origins(&bytes, None, &fresh.tys, alloc(), &mut |key, origin| {
            references.push((key, origin));
            Ok(())
        }).expect("decode and register the references");
        assert_eq!(references, [(opaque, expected)], "{at:?} must register the alias's opaque");
        let reopened = head_record::decode_retaining(&bytes, None, &fresh.tys, alloc()).expect("standalone reopened record");
        let rewritten = reopened.encode(fresh.tys.view(), alloc()).expect("write from the reader's type table");
        assert_eq!(&rewritten[..], &bytes[..], "{at:?}");
    }
    println!("current head records: Holder, Hidden and opaque #0 retain dep::Hidden and round-trip byte for byte");
}

#[test]
fn alias_head_round_trips_its_opaque_bounds() {
    let mut fixture = Fixture::new();
    let alias = fixture.key(1, "Hidden");
    let want = Want::Head(alias);
    let owner = checker_of(alias);
    let opaque = opaque_key(owner, 0);
    let bytes = fixture.bytes(want).clone();
    let frozen = FrozenHead::read(&bytes).expect("fresh head");
    assert_eq!(frozen.heads().map(|e| e.at).collect::<Vec<_>>(), [HeadAt::Alias(alias), HeadAt::Opaque(opaque)]);
    for e in frozen.heads() {
        assert_eq!(&e.record[4..8], &head_record::VERSION.to_le_bytes());
    }
    let front = Front2::from_records(fixture.records, fixture.frame, LANG).expect("reader front end");
    let mut store = Store::new(front.index.lang("sized"), fixture.frame, alloc());
    let table = fixture.table();
    let seed = table.view(fixture.records);
    seed.seed(&store, &front.keys, Want::Adt(fixture.key(1, "Holder")), alloc()).expect("restore the field's head");
    assert!(store.preds.get(&opaque).is_none(), "only the defining head publishes the bounds");
    seed.seed(&store, &front.keys, want, alloc()).expect("restore the defining head");
    assert_eq!(store.opaques.get(&opaque), Some(&(owner, 1u32 << 31)));
    let predicates = store.preds.get(&opaque).expect("opaque predicates");
    for bound in ["Send", "Fn"] {
        let expected = checker_of(fixture.key(2, bound));
        assert!(predicates.clauses.iter().any(|p| matches!(p, blaze_typeck::PClause::Trait { trait_, .. } if *trait_ == expected)), "missing declared bound {bound}");
    }
    let types = store.tys.view();
    assert!(predicates.clauses.iter().any(|p| matches!(p,
        blaze_typeck::PClause::Projection { name, term, .. }
            if *name == blaze_typeck::Name::of("Output") && matches!(types.node(*term), Ok(blaze_typeck::PNode::Uint(blaze_typeck::UintTy::U8)))
    )), "Fn has the written output u8");
    let restored = head::export(fixture.records, &front.keys, &mut store, want, Plant::None, alloc()).expect("re-export");
    assert_eq!(&restored[..], &bytes[..]);
    println!("dep::Hidden: opaque #0, Fn and Send bounds; current bytes restored");
}

/// Keep the alias record byte-identical and remove only its second directory entry and
/// payload. Offsets are payload-relative, so the first row still starts at offset zero.
#[test]
fn missing_alias_opaque_row_names_its_definition() {
    let mut fixture = Fixture::new();
    let alias = fixture.key(1, "Hidden");
    let opaque = opaque_key(checker_of(alias), 0);
    let bytes = fixture.bytes(Want::Head(alias));
    let rows = FrozenHead::read(bytes).unwrap().heads().collect::<Vec<_>>();
    assert_eq!(rows.iter().map(|r| r.at).collect::<Vec<_>>(), [HeadAt::Alias(alias), HeadAt::Opaque(opaque)]);
    let header = 8 + head::WANT_BYTES + 4;
    let dir_len = bytes.len() - header - rows.iter().map(|e| e.record.len()).sum::<usize>();
    let mut stripped = bytes[..header + dir_len / 2].to_vec();
    stripped[header - 4..header].copy_from_slice(&1u32.to_le_bytes());
    stripped.extend_from_slice(rows[0].record);
    *bytes = stripped;
    let refusal = match fixture.task("direct") {
        Err(error) => error,
        Ok(_) => panic!("the missing promised row must refuse the body's fact"),
    };
    let TaskError::Cell { key, holder, ask, .. } = &refusal else { panic!("{refusal}") };
    assert_eq!((*key, *holder), (opaque, checker_of(alias)));
    let why = refusal.to_string();
    match ask {
        CellAsk::Fact(kind) => assert!(why.contains(&format!("{kind:?}")), "{why}"),
        CellAsk::Several { first, another } => {
            assert!(why.contains(&format!("{first:?}")) && why.contains(&format!("{another:?}")), "{why}");
        }
        CellAsk::SeedBeforeFact | CellAsk::SeedWithoutReads(_) => panic!("the body asked about the field's opaque: {why}"),
    }
    let prefix = "pub type Hidden = ";
    let term = "impl Fn() -> u8 + Send";
    let line = DEP.lines().position(|line| line.starts_with(prefix)).unwrap() + 1;
    let place = format!("src/lib.rs:{line}:{}", prefix.len() + 1);
    let relative = format!("bytes {}..{} in the type alias", prefix.len(), prefix.len() + term.len());
    let key = format!("{:032x}", opaque.0);
    for part in ["dep", "Hidden", "opaque type #0", &place, &relative, term, &key] {
        assert!(why.contains(part), "missing {part:?} in {why}");
    }
    fixture.clean("scalar");
    let orphan = blaze_ladder::front2::item_shown::FactDefinition::of(fixture.records, opaque, None, alloc()).to_string();
    for part in ["opaque type #0", "dep", "Hidden", &place, term] {
        assert!(orphan.contains(part), "the orphan key alone must identify its definition: {orphan}");
    }
    println!("{why}");
    println!("unrelated scalar: clean");
}

#[test]
fn a_scalar_alias_cannot_fulfil_an_already_imported_opaque_reference() {
    let mut fixture = Fixture::new();
    let want = Want::Head(fixture.key(1, "Hidden"));
    let holder = Want::Adt(fixture.key(1, "Holder"));
    let opaque = opaque_key(checker_of(want.key()), 0);
    // Publish an internally consistent alias from a different edit under the same item
    // identity. It has neither an opaque row nor a term that could remind the importer of it.
    let scalar_text = DEP.replace("pub type Hidden = impl Fn() -> u8 + Send;", "pub type Hidden = u8;");
    let scalar = Fixture::solve(Box::leak(scalar_text.into_boxed_str()));
    assert_eq!(scalar.key(1, "Hidden"), want.key());
    let (mut front, mut producer) = scalar.producer();
    producer.ensure(scalar.names, &mut front.keys, want).unwrap();
    *fixture.bytes(want) = head::export(scalar.names, &front.keys, &mut producer, want, Plant::None, alloc()).expect("scalar alias bytes").to_vec();
    let table = fixture.table();
    let seed = table.view(fixture.records);
    let reader = Front2::from_records(fixture.records, fixture.frame, LANG).unwrap();
    let store = Store::new(reader.index.lang("sized"), fixture.frame, alloc());
    seed.seed(&store, &reader.keys, holder, alloc()).expect("register the holder's field reference");
    let refused = seed.seed(&store, &reader.keys, want, alloc()).expect_err("the alias owes the promised opaque row");
    assert!(matches!(refused, SeedError::Freeze(FreezeError::Record {
        error: HeadRecordError::MissingOpaqueRow { key, origin }, ..
    }) if key == opaque && origin.want == want), "{refused:?}");
    println!("dep::Hidden: scalar alias refused for missing previously promised opaque #0");
}

#[test]
fn forged_opaque_owner_is_refused() {
    let mut fixture = Fixture::new();
    let owner = checker_of(fixture.key(1, "Hidden"));
    let opaque = opaque_key(owner, 0);
    let want = Want::Adt(fixture.key(1, "Holder"));
    let bytes = fixture.bytes(want);
    let owner_bytes = owner.0.to_le_bytes();
    let positions: Vec<_> = bytes.windows(owner_bytes.len()).enumerate().filter_map(|(i, s)| (s == owner_bytes).then_some(i)).collect();
    assert_eq!(positions.len(), 1, "the holder has one reference to the checker owner");
    bytes[positions[0]] ^= 1;
    let table = fixture.table();
    let front = Front2::from_records(fixture.records, fixture.frame, LANG).expect("records front end");
    let store = Store::new(front.index.lang("sized"), fixture.frame, alloc());
    let refusal = table.view(fixture.records).seed(&store, &front.keys, want, alloc()).expect_err("forged owner must be refused");
    assert!(matches!(refusal, SeedError::Freeze(FreezeError::Record { error: HeadRecordError::InvalidOpaqueOrigin { key, .. }, .. }) if key == opaque), "{refusal:?}");
    assert!(store.opaques.get(&opaque).is_none());
    println!("dep::Holder: forged checker owner for opaque #0 refused");
}

#[test]
fn old_frozen_and_inner_versions_request_a_rebuild() {
    let mut fixture = Fixture::new();
    let want = Want::Head(fixture.key(1, "Hidden"));
    let mut bytes = fixture.bytes(want).clone();
    let mut record = FrozenHead::read(&bytes).unwrap().heads().next().unwrap().record.to_vec();
    bytes[4..8].copy_from_slice(&(head::VERSION - 1).to_le_bytes());
    assert!(matches!(FrozenHead::read(&bytes), Err(head::FrozenError::Version { found }) if found == head::VERSION - 1));
    record[4..8].copy_from_slice(&(head_record::VERSION - 1).to_le_bytes());
    let store = Store::new(None, fixture.frame, alloc());
    assert!(matches!(head_record::decode(&record, None, &store.tys, alloc()), Err(HeadRecordError::Version { found }) if found == head_record::VERSION - 1));
    println!("frozen and inner records of an older version require a rebuild");
}

/// D161 step 3's oracle (front2 tables/task.rs `compare_asks`) on seeded bodies, which import
/// heads on the spot: the restart path (`AsksMet::Again`) and the one ask (`AsksMet::Once`)
/// answer each body alike. CONTROL: with `AsksPlant::RefuseMade` (a made head answered as
/// refused) the oracle sees the bodies that import a head differ, so it can fail; with it
/// clean, nothing differs.
#[test]
fn the_one_ask_answers_seeded_bodies_as_the_restart_path_and_its_plant_is_seen() {
    use blaze_ladder::front2::asks::AsksPlant;
    use blaze_ladder::front2::tables::task::compare_asks;
    let fixture = Fixture::new();
    let recorder = Recorder::new(RecorderOpts { alloc: alloc() });
    let table = fixture.frame.alloc(fixture.table());
    let seed = fixture.frame.alloc(table.view(fixture.records));
    let tables = Tables::new(TablesOpts { records: fixture.records, impls: None, lang_crates: LANG, reads: &recorder });
    let frame = BodyFrame { frame: fixture.frame, alloc: alloc(), needs: NeedsMet::All, heads: Some(seed), cost: None, plant_drop: None, sources: None, goals: None, prepared: None };
    let mut seen = 0;
    for name in ["direct", "nested", "scalar"] {
        let task = Task { planned: Planned { name: Name::new(Kind::BodyCheck, fixture.key(0, name).0), phase: Phase::new(Stage::Bodies, 0), source: ABSENT, file: Name::new(Kind::FileItems, ABSENT) }, key: Key(ABSENT) };
        if let Err(differ) = compare_asks(tables, &task, &frame, alloc(), AsksPlant::Clean) {
            panic!("{name}: {differ}");
        }
        match compare_asks(tables, &task, &frame, alloc(), AsksPlant::RefuseMade) {
            Err(differ) => {
                seen += 1;
                println!("{name}: the planted refusal was seen:\n{differ}");
            }
            // A body that imports no head on the spot cannot meet the plant (`scalar`).
            Ok(()) => println!("{name}: imports no head on the spot; the plant has nothing to refuse"),
        }
    }
    assert!(seen >= 2, "the planted refusal must change `direct` and `nested`, which import dep's heads; seen in {seen}");
    println!("one ask equal to the restart path on 3 seeded bodies; the plant seen in {seen}");
}

/// The run memo's oracle (front2 tables/task.rs `compare_repeats`; recorder.rs `Repeats`): each
/// seeded body checked by a recorder that offers a repeated question's reads again and by one
/// that mutes them gives one verdict and one read list, byte for byte; the muted side offers
/// exactly what the other offered less its memo, and the memo is met (the syntactic prefetch
/// answers every path once before the checker asks it). CONTROL: with
/// `RepeatsPlant::DropFirst` (the muted side drops its first names read) the oracle sees the
/// lists differ in every body that reads a names cell.
#[test]
fn a_repeated_question_offers_nothing_and_leaves_each_read_list_equal_and_its_plant_is_seen() {
    use blaze_ladder::front2::tables::task::{RepeatsPlant, compare_repeats};
    let fixture = Fixture::new();
    let recorder = Recorder::new(RecorderOpts { alloc: alloc() });
    let table = fixture.frame.alloc(fixture.table());
    let seed = fixture.frame.alloc(table.view(fixture.records));
    let tables = Tables::new(TablesOpts { records: fixture.records, impls: None, lang_crates: LANG, reads: &recorder });
    let frame = BodyFrame { frame: fixture.frame, alloc: alloc(), needs: NeedsMet::All, heads: Some(seed), cost: None, plant_drop: None, sources: None, goals: None, prepared: None };
    let (mut memo, mut seen) = (0u64, 0u32);
    for name in ["direct", "nested", "scalar"] {
        let task = Task { planned: Planned { name: Name::new(Kind::BodyCheck, fixture.key(0, name).0), phase: Phase::new(Stage::Bodies, 0), source: ABSENT, file: Name::new(Kind::FileItems, ABSENT) }, key: Key(ABSENT) };
        let (offer, quiet) = match compare_repeats(tables, &task, &frame, alloc(), RepeatsPlant::Clean) {
            Ok(tallies) => tallies,
            Err(differ) => panic!("{name}: {differ}"),
        };
        assert_eq!(offer.memo, 0, "{name}: the offering side mutes nothing");
        assert_eq!(quiet.offered + quiet.memo, offer.offered, "{name}: the muted side offers what the other did less its memo ({offer:?} against {quiet:?})");
        assert_eq!(quiet.kept, offer.kept, "{name}: both sides keep one list");
        memo += quiet.memo;
        println!("{name}: offered {} -> {} (memo {}, filtered {}), kept {}", offer.offered, quiet.offered, quiet.memo, quiet.filtered, quiet.kept);
        match compare_repeats(tables, &task, &frame, alloc(), RepeatsPlant::DropFirst) {
            Err(differ) => {
                seen += 1;
                println!("{name}: the planted drop was seen:\n{differ}");
            }
            Ok(_) => println!("{name}: reads no names cell; the plant has nothing to drop"),
        }
    }
    assert!(memo > 0, "the prefetch answers every path before the checker asks it: some question must repeat");
    assert!(seen >= 2, "the planted drop must change `direct` and `nested`, which read names; seen in {seen}");
    println!("repeats: 3 seeded bodies' read lists equal with the memo on, {memo} offers muted; the plant seen in {seen}");
}

/// A fixture handed whole to the one thread that checks on a small stack.
struct Handed(Fixture);

// SAFETY: `Records`, `Names` and `Frame` are not `Sync` (cells, a bump cursor) because two
// threads must not use one at once. The test makes the fixture, hands it to exactly one
// spawned thread and then only joins it: nothing of it is touched on the test thread until the
// join returns, so every use is from one thread at a time, ordered by the join. None holds a
// reference into a thread-local.
unsafe impl Send for Handed {}

/// The stack a body check costs, from the body task down to the type checker (front2
/// tables/task.rs `record_body_as`, project.rs `check_then`, check.rs `check_rounds_as`,
/// blaze-typeck `run`, and the heads a seed makes on the way: head.rs `freeze_noting`,
/// heads.rs `step` and `fn_head`, types.rs `path_regions`). blaze-cli
/// `a_changed_archive_checksum_enters_the_body_through_the_graph` overflowed its 2 MiB test
/// thread there (blaze-int notes t29 overflow-bt4.txt): those eight debug frames held 532,560
/// bytes, `record_body_as` alone 156,400, each holding a world, a store, an inference context, a
/// machine or a lowered body by value. Oracle: the body task of `direct` (a call whose callee's
/// bound names dep's opaque) runs on a thread of [`SMALL_STACK`], from a fresh fixture, and
/// gives the verdict digest and read list it gives on the test thread. Control (rule 23): on the
/// test's own stack it is clean and reads dep's `Hidden` head, so the small thread is not
/// passed by a body that checks nothing.
const SMALL_STACK: usize = 512 << 10;

#[test]
fn a_seeded_body_checks_on_a_small_stack() {
    let fixture = Fixture::new();
    let control = fixture.clean("direct");
    let alias = checker_of(fixture.key(1, "Hidden")).0.to_be_bytes();
    assert!(control.reads.contains(&TableRead::Head { item: alias, part: 0 }), "CONTROL: `direct` must read dep's `Hidden` head");
    let want: Vec<TableRead> = control.reads.iter().copied().collect();
    let handed = Handed(Fixture::new());
    let small = std::thread::Builder::new()
        .name(String::from("body-on-a-small-stack"))
        .stack_size(SMALL_STACK)
        .spawn(move || {
            let h = handed;
            let checked = h.0.clean("direct");
            (checked.out, checked.reads.iter().copied().collect::<Vec<TableRead>>())
        })
        .unwrap_or_else(|e| panic!("spawn a thread of {SMALL_STACK} bytes: {e}"));
    let (out, reads) = small.join().unwrap_or_else(|_| panic!("ORACLE: checking `direct` on a {SMALL_STACK}-byte stack failed"));
    assert_eq!(out, control.out, "ORACLE: the small stack's verdict differs");
    assert_eq!(reads, want, "ORACLE: the small stack's read list differs");
    println!("`direct` checked on a {SMALL_STACK}-byte stack: {} reads, equal to the test thread's", reads.len());
}
