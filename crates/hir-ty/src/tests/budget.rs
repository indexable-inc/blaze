//! What inference allocates (docs/v2/plan/lanes/mem/db-infer.md, "Budget tests"): nothing on a
//! re-check of bodies whose inputs did not change, its owned result alone on a run whose
//! dependencies are all memoized, and frame memory linear in the body.

use base_db::{Db, all_crates};
use blaze_alloc_count::{assert_at_most, measure};
use blaze_mem::{Tag, frame};
use hir_def::{AssocItemId, DefWithBodyId, LoweringMode, ModuleDefId, nameres::crate_def_map};
use blaze_query::db::{Database, Durability};
use hir_expand::fixture;

use crate::{InferenceResult, infer::infer_body_in};

/// The closures' body of [`FIXTURE`], in both of its crates: captures through a deref (`FnMut`),
/// a fake read with a guard, a nested closure and a coroutine-closure.
macro_rules! closures {
    () => {
        r#"
struct Text(u8);
impl Text { fn len(&self) -> usize { self.0 as usize } }
struct Bytes(u8);
impl Bytes { fn len(&self) -> usize { self.0 as usize } }

struct S { a: (Text, u8), b: Bytes }
impl S {
    fn closures(&mut self, n: Option<u8>) -> u8 {
        let mut f = || { self.a.1 += 1; self.b.len() };
        let g = move || match n { Some(x) if x > 1 => x, _ => 0 };
        let h = || { let k = || self.a.0.len(); k() };
        let a = async || self.a.1;
        let _a = a;
        f() as u8 + g() + h() as u8
    }
}
"#
    };
}

const FIXTURE: &str = concat!(
    r#"
//- minicore: sized, copy, clone, add, mul, fn, option, deref, async_fn, future
//- /main.rs crate:main deps:core
struct Point { x: u32, y: u32 }

impl Point {
    fn new(x: u32, y: u32) -> Point { Point { x, y } }
    fn sum(&self) -> u32 { self.x + self.y }
}

enum Shape { Dot(Point), Line(Point, Point) }

fn area(shape: &Shape) -> u32 {
    match shape {
        Shape::Dot(p) => p.sum(),
        Shape::Line(a, b) => {
            let d = a.sum() + b.sum();
            d * 2
        }
    }
}

fn first(points: &(Point, Point)) -> Option<&Point> {
    if points.0.x > points.1.x { Some(&points.0) } else { None }
}

fn main() {
    let p = Point::new(1, 2);
    let shapes = (Shape::Dot(p), Shape::Line(Point::new(3, 4), Point::new(5, 6)));
    let total = area(&shapes.0) + area(&shapes.1);
    let add = |a: u32| a + total;
    let _ = add(7);
    let pair = (Point::new(1, 1), Point::new(2, 2));
    let _ = first(&pair).map(|it| it.sum());
}
"#,
    closures!(),
    r#"
//- /old.rs crate:old edition:2018 deps:core
"#,
    closures!(),
);

fn db_of(fixture: &str) -> Db {
    let db = fixture::db(fixture).into_db();
    db
}

/// Every body of the fixture's own crates (not minicore's): functions, consts and statics in
/// modules and impls.
fn bodies(db: &Db) -> Vec<DefWithBodyId> {
    let mut bodies = Vec::new();
    for &krate in all_crates(db).iter() {
        if !krate.data(db).origin.is_local() {
            continue;
        }
        let def_map = crate_def_map(db, krate);
        for (_, module) in def_map.modules(db) {
            let scope = module.scope(db);
            for def in scope.declarations() {
                match def {
                    ModuleDefId::FunctionId(it) => bodies.push(it.into()),
                    ModuleDefId::ConstId(it) => bodies.push(it.into()),
                    ModuleDefId::StaticId(it) => bodies.push(it.into()),
                    _ => {}
                }
            }
            for it in scope.impls() {
                for &(_, item) in it.impl_items(db).items.iter() {
                    match item {
                        AssocItemId::FunctionId(it) => bodies.push(it.into()),
                        AssocItemId::ConstId(it) => bodies.push(it.into()),
                        AssocItemId::TypeAliasId(_) => {}
                    }
                }
            }
        }
    }
    assert!(!bodies.is_empty(), "the fixture has bodies");
    bodies
}

fn infer_all(db: &Db, bodies: &[DefWithBodyId]) {
    for &body in bodies {
        let _ = InferenceResult::of(db, body);
    }
}

#[test]
fn a_recheck_of_unchanged_bodies_allocates_nothing() {
    let mut db = db_of(FIXTURE);
    let bodies = bodies(&db);
    infer_all(&db, &bodies);
    // The first re-check after a write warms what verification keeps per worker; the second is
    // the steady state.
    db.storage_mut().synthetic_write(Durability::LOW);
    infer_all(&db, &bodies);
    db.storage_mut().synthetic_write(Durability::LOW);
    assert_at_most(0, "re-checking the inference of unchanged bodies", || {
        infer_all(&db, &bodies)
    });
}

#[test]
fn a_cold_infer_allocates_its_result_alone() {
    let db = db_of(FIXTURE);
    let bodies = bodies(&db);
    // Every query inference asks is memoized and every type it makes interned: what the run
    // below allocates is its own.
    infer_all(&db, &bodies);
    for &body in &bodies {
        // The memo's copy is measured first, so a failure's traces (`BLAZE_ALLOC_TRACE=1`) are
        // the run's.
        let (memo, owned) = measure(|| InferenceResult::of(&db, body).clone());
        frame::task(Tag::Infer, |frame| {
            let (result, used) =
                measure(|| infer_body_in(&db, body, None, LoweringMode::Analysis, frame));
            assert!(memo == result, "a run gives the memoized result: {body:?}");
            assert!(
                used.allocations <= owned.allocations,
                "{body:?}: inference made {} allocations ({} bytes), its result owns {} ({} bytes)",
                used.allocations,
                used.bytes,
                owned.allocations,
                owned.bytes,
            );
        });
    }
}

/// `fn chain() -> u32` of `lets` statements, each adding one to the last.
fn chain(lets: usize) -> String {
    let mut text = String::from("//- minicore: sized, copy, add\nfn chain() -> u32 {\n    let x0: u32 = 1;\n");
    for at in 1..lets {
        text.push_str(&format!("    let x{at} = x{} + 1;\n", at - 1));
    }
    text.push_str(&format!("    x{}\n}}\n", lets - 1));
    text
}

/// The frame bytes a run of the inference of `chain(lets)` uses at most.
fn peak_of_chain(lets: usize) -> usize {
    let db = db_of(&chain(lets));
    let bodies = bodies(&db);
    assert_eq!(bodies.len(), 1);
    infer_all(&db, &bodies);
    frame::task(Tag::Infer, |frame| {
        let _ = infer_body_in(&db, bodies[0], None, LoweringMode::Analysis, frame);
        frame.peak()
    })
}

#[test]
fn frame_use_is_linear_in_the_body() {
    let small = peak_of_chain(200);
    let large = peak_of_chain(800);
    assert!(small > 0, "inference's temporaries are frame values");
    // Four times the statements: at most four times the bytes and the fixed part again.
    assert!(large <= 4 * small + small / 2, "frame peak {small} bytes for 200 lets, {large} for 800");
}
