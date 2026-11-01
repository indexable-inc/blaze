//! A part written in one database and read in another, whose ids differ; the bytes of a
//! record outside a scope; the key; damaged parts; what finding a record allocates.

use std::sync::Arc;

use base_db::{Crate, Db, all_crates};
use blaze_alloc_count::assert_at_most;
use blaze_format::{Aligned, HEADER_LEN, Persist};
use blaze_hash::HashMap;
use blaze_query::db::AsId;
use hir_def::{
    DefWithBodyId, FunctionId, LoweringMode, deps::ImageBuf, nameres::demand::crate_body_owners,
    signatures::FunctionSignature,
};
use hir_expand::fixture;
use hir_expand::name::Name;

use super::{ids::Table, part, *};
use crate::{
    InferenceResult,
    infer::{infer_query_with_inspect, persist::InferenceRecord},
    next_solver::DbInterner,
};

/// A dependency with closures, a coroutine closure, an `async fn`, an opaque type, constants in
/// types and in blocks, a `dyn` type with auto traits, derives; [`PAD`] comes first in the
/// second database, so the dependency's ids there are not the first one's.
const FIXTURE: &str = r#"
//- minicore: sized, copy, clone, add, mul, fn, option, deref, async_fn, future, derive, eq, send, sync
//- /other.rs crate:other deps:core new_source_root:library
pub struct O(pub u32);
pub fn o1() -> u32 { 1 }
pub fn o2(o: O) -> u32 { o.0 + o1() }
pub fn o3() -> u32 { let f = |x: u32| x * 2; f(o1()) }
//- /dep.rs crate:dep deps:core new_source_root:library
#[derive(Clone, Copy, PartialEq)]
pub struct Point { pub x: u32, pub y: u32 }
impl Point {
    pub fn sum(&self) -> u32 { self.x + self.y }
}
pub enum Shape { Dot(Point), Line(Point, Point) }
pub fn area(shape: &Shape) -> u32 {
    match shape {
        Shape::Dot(p) => p.sum(),
        Shape::Line(a, b) => a.sum() + b.sum(),
    }
}
pub fn adder(n: u32) -> impl Fn(u32) -> u32 { move |x| x + n }
pub fn use_adder() -> u32 { adder(1)(2) }
pub fn arrays() -> u8 { let a = [1u8; 4]; let b: [u8; 2 + 2] = a; let [x, ..] = b; x }
pub fn inline_const() -> u32 { const { 5 } }
pub async fn later(x: u32) -> u32 { x }
pub fn coroutine_closure() -> u32 { let f = async || 1u32; let _g = f; 0 }
pub fn call(f: &(dyn Fn() -> u32 + Send + Sync)) -> u32 { f() }
pub fn copies(p: Point) -> (Point, bool) { (p.clone(), p == p) }
pub fn nested(p: Point) -> u32 { let outer = || { let inner = || p.x; inner() }; outer() }
pub const C: u32 = 3 + 4;
pub static S: u32 = C * 2;
pub enum E { A = 1, B = C as isize }
//- /main.rs crate:main deps:core,dep,other new_source_root:local
fn main() { let _ = dep::area(&dep::Shape::Dot(dep::Point { x: 1, y: 2 })); }
"#;

fn db_of(fixture: &str) -> Db {
    let db = fixture::db(fixture).into_db();
    db
}

fn crate_named(db: &Db, name: &str) -> Crate {
    *all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name))
        .unwrap()
}

fn function(db: &Db, krate: Crate, name: &str) -> FunctionId {
    crate_body_owners(db, krate)
        .into_iter()
        .find_map(|def| match def {
            DefWithBodyId::FunctionId(it) if FunctionSignature::of(db, it).name == Name::new_root(name) => {
                Some(it)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no function `{name}`"))
}

/// `dep`'s part, written in a database that knows `dep` first.
fn written() -> Vec<u8> {
    let db = db_of(FIXTURE);
    let dep = crate_named(&db, "dep");
    encode(&db, dep, &|_| true).unwrap()
}

/// A second database, [`FIXTURE`] with a crate `pad` before its own, and `dep` in it. Name
/// resolution interns every crate's items as the database is made, in the crate graph's order,
/// so only items made before `dep`'s can move its ids, not what is asked of the database
/// afterwards.
fn second() -> (Db, Crate) {
    let at = FIXTURE.find("//- /").expect("a fixture with a file");
    let db = db_of(&format!("{}{PAD}{}", &FIXTURE[..at], &FIXTURE[at..]));
    let dep = crate_named(&db, "dep");
    (db, dep)
}

/// A crate of nothing the fixture's crates name, its functions interned before theirs.
const PAD: &str = "//- /pad.rs crate:pad deps:core new_source_root:library
pub fn p1() {}
pub fn p2() {}
pub fn p3() {}
pub struct P;
impl P { pub fn p4(&self) {} }
";

fn buf(bytes: &[u8]) -> ImageBuf {
    Arc::new(Aligned::copy_from(bytes))
}

#[test]
fn a_part_reads_back_in_another_database() {
    let bytes = written();
    let first = db_of(FIXTURE);
    let first_dep = crate_named(&first, "dep");
    let (mut db, dep) = second();
    let bits = |db: &Db, krate| function(db, krate, "area").as_id().as_u32();
    assert_ne!(bits(&first, first_dep), bits(&db, dep), "the two databases' ids differ");

    open(&bytes).unwrap();
    // alloc: test setup
    let mut parts = HashMap::default();
    parts.insert(dep, buf(&bytes));
    set_served(&mut db, parts);

    let owners = crate_body_owners(&db, dep);
    assert!(owners.len() >= 15, "the fixture's bodies: {owners:?}");
    for def in owners {
        let served = served_inference(&db, def).unwrap_or_else(|| panic!("{def:?} is served"));
        let own = infer_query_with_inspect(&db, def, None, LoweringMode::Analysis);
        assert!(served == own, "{def:?}: served {served:?}\nown {own:?}");
        assert!(served_passed(&db, def), "{def:?}'s verdict");
    }
}

#[test]
fn outside_a_scope_a_record_is_todays_bytes() {
    let db = db_of(FIXTURE);
    let dep = crate_named(&db, "dep");
    let area = function(&db, dep, "area");
    let sum = function(&db, dep, "sum");
    let interner = DbInterner::new_with(&db, dep);
    let result = InferenceResult::of(&db, area);

    let outside = blaze_format::to_bytes(&result.to_record(interner));
    assert_eq!(outside, blaze_format::to_bytes(&result.to_record(interner)), "deterministic");
    let read = InferenceRecord::read(&Aligned::copy_from(&outside)).unwrap().to_result(interner);
    assert!(read.unwrap() == *result, "a record outside a scope reads back in its own database");
    let sum_bits = u64::from(sum.as_id().as_u32()).to_le_bytes();
    let names = |bytes: &[u8]| bytes.windows(8).any(|it| it == sum_bits);
    assert!(names(&outside), "a method's id, as today's records write it");

    let mut table = Table::default();
    let inside = ids::relocating(&mut table, || blaze_format::to_bytes(&result.to_record(interner)));
    assert!(table.len() > 0, "the record names ids");
    assert_ne!(inside, outside, "inside a scope, ids are written as their index in the table");
}

#[test]
fn an_edit_to_the_dependency_moves_the_part_key() {
    let key_of = |fixture: &str| {
        let db = db_of(fixture);
        let dep = crate_named(&db, "dep");
        let image = hir_def::deps::keys(&db, &[dep], blaze_ids::ContentId([7; 32]))[&dep];
        key(image, 0)
    };
    let before = key_of(FIXTURE);
    assert_eq!(before, key_of(FIXTURE), "the same in every database");
    assert_ne!(before, key_of(&FIXTURE.replace("pub const C: u32 = 3 + 4;", "pub const C: u32 = 3 + 5;")));
    let image = blaze_ids::ContentId([1; 32]);
    assert_ne!(key(image, 0), key(image, 1), "the borrow checker's mode");
}

/// What every body of `bytes` reads as, by key: its verdict, or why it is refused.
fn bodies(bytes: &[u8]) -> Vec<(blaze_ids::ContentId, Result<bool, String>)> {
    let part = part::view(bytes).unwrap();
    part.keys()
        .iter()
        .map(|key| (*key, part.find(key).ok_or_else(String::new).and_then(part::open_body).map(|it| it.passed())))
        .collect()
}

#[test]
fn damaged_parts_are_refused_within_bounds() {
    let bytes = written();
    let original = Aligned::copy_from(&bytes);
    part::open(&original).unwrap();
    let reads = bodies(&original);
    let body_bytes = |bytes: &[u8], key: &blaze_ids::ContentId| {
        part::view(bytes).ok()?.find(key).map(<[u8]>::to_vec)
    };

    for len in 0..bytes.len() {
        let cut = Aligned::copy_from(&bytes[..len]);
        assert!(part::open(&cut).is_err(), "cut to {len} bytes");
    }
    for at in 0..HEADER_LEN {
        let mut damaged = bytes.clone();
        damaged[at] ^= 0xff;
        assert!(part::open(&Aligned::copy_from(&damaged)).is_err(), "header byte {at} flipped");
    }
    // The columns: after the last body, up to the root and through it.
    let last = part::view(&original).unwrap().keys().iter().filter_map(|key| {
        let found = part::view(&original).unwrap().find(key)?;
        Some(found.as_ptr() as usize - original.as_ptr() as usize + found.len())
    });
    let columns = last.max().unwrap_or(HEADER_LEN);
    let (db, dep) = second();
    for at in columns..bytes.len() {
        let mut damaged = bytes.clone();
        damaged[at] ^= 0xff;
        let damaged = Aligned::copy_from(&damaged);
        let Ok(part) = part::open(&damaged) else { continue };
        let _ = part.items(&db, dep);
        for (key, read) in &reads {
            let found = body_bytes(&damaged, key);
            let same = found.is_none()
                || found == body_bytes(&original, key)
                || found.as_deref().is_some_and(|it| part::open_body(it).is_err());
            assert!(same, "byte {at} flipped: {key:?} read as another body ({read:?} before)");
        }
    }
    for (key, read) in reads {
        assert_eq!(read, Ok(true), "{key:?}");
    }
}

#[test]
fn a_damaged_record_is_refused_by_its_read() {
    let bytes = written();
    let (db, dep) = second();
    let original = Aligned::copy_from(&bytes);
    let part = part::open(&original).unwrap();
    let items = part.items(&db, dep).unwrap();
    let key = part.keys()[0];
    let body = part.find(&key).unwrap();
    let start = body.as_ptr() as usize - original.as_ptr() as usize;
    for at in 0..HEADER_LEN {
        let mut damaged = bytes.clone();
        damaged[start + at] ^= 0xff;
        let damaged = Aligned::copy_from(&damaged);
        let body = part::view(&damaged).unwrap().find(&key).unwrap();
        let read = part::open_body(body).and_then(|it| it.inference(&db, dep, &items).map(drop));
        assert!(read.is_err(), "the body's header byte {at} flipped");
    }
}

/// A served crate's body whose key the part neither records nor lists as left out: here one
/// the writing database did not have, as a body whose key differed between the two would be.
#[test]
fn a_body_neither_recorded_nor_left_out_is_an_error() {
    let bytes = written();
    let part = part::open(&bytes).unwrap();
    assert!(part.len() > 0);
    let fixture = FIXTURE.replace("pub const C: u32", "pub fn unwritten() -> u32 { 9 }\npub const C: u32");
    let mut db = db_of(&fixture);
    let dep = crate_named(&db, "dep");
    // alloc: test setup
    let mut parts = HashMap::default();
    parts.insert(dep, buf(&bytes));
    set_served(&mut db, parts);
    let unwritten = DefWithBodyId::FunctionId(function(&db, dep, "unwritten"));
    let key = hir_def::deps::body_key(&db, unwritten).expect("a module's function has a key");
    assert!(part.find(&key).is_none() && part.left_out(&key).is_none());
    for read in [
        &(|| drop(served_inference(&db, unwritten))) as &dyn Fn(),
        &|| drop(served_passed(&db, unwritten)),
    ] {
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(read)).unwrap_err();
        let message = panic.downcast_ref::<String>().map(String::as_str).unwrap_or_default();
        assert!(message.contains("neither recorded nor left out"), "{message}");
    }
    // The bodies the part has are still served.
    let area = DefWithBodyId::FunctionId(function(&db, dep, "area"));
    assert!(served_inference(&db, area).is_some());
}

/// A body the writer left out is listed with why, and reads as not served, not as an error.
#[test]
fn a_left_out_body_is_listed_with_why_and_not_served() {
    let first = db_of(FIXTURE);
    let first_dep = crate_named(&first, "dep");
    let first_area = DefWithBodyId::FunctionId(function(&first, first_dep, "area"));
    // alloc: test setup
    let leave = |def: DefWithBodyId| (def == first_area).then(|| "left out by the test".to_owned());
    let bytes = part::encode(&first, first_dep, &|_| true, &leave).unwrap();
    let part = part::open(&bytes).unwrap();
    let (mut db, dep) = second();
    let area = DefWithBodyId::FunctionId(function(&db, dep, "area"));
    let key = hir_def::deps::body_key(&db, area).unwrap();
    assert_eq!(part.left_out(&key), Some("left out by the test"));
    assert!(part.find(&key).is_none());
    // alloc: test setup
    let mut parts = HashMap::default();
    parts.insert(dep, buf(&bytes));
    set_served(&mut db, parts);
    assert!(served_inference(&db, area).is_none() && !served_passed(&db, area));
    let sum = DefWithBodyId::FunctionId(function(&db, dep, "sum"));
    assert!(served_inference(&db, sum).is_some(), "the others are served");
}

#[test]
fn finding_a_record_allocates_nothing() {
    let bytes = Aligned::copy_from(&written());
    let keys = part::open(&bytes).unwrap().keys().to_vec();
    assert_at_most(0, "finding the record of every body of a part", || {
        for key in &keys {
            let part = part::view(&bytes).unwrap();
            let body = part.find(key).unwrap();
            assert!(part::open_body(body).unwrap().passed());
        }
    });
}
