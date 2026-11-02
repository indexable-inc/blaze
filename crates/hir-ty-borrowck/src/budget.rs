//! What the borrow checker allocates on the global heap once the thread's frame is warm: for
//! a clean body, typical or large, nothing, and nothing for another body of no greater size
//! after it; for a body with errors, the diagnostics it returns and nothing else; to answer
//! again from the memo, nothing. And the fixpoint alone: composing, solving and reading a large
//! body's analyses allocates nothing.

use std::fmt::Write as _;

use base_db::{Db, all_crates};
use blaze_alloc_count::{assert_at_most, measure};
use blaze_ids::rng::SplitMix64;
use blaze_mem::{Tag, frame};
use hir_def::{DefWithBodyId, ModuleDefId, nameres::crate_def_map, signatures::FunctionSignature};
use hir_ty_mir as _;
use hir_expand::fixture;

use crate::{
    BorrowckResult, PartSpan, borrowck, borrowck_in_mode,
    differential::{self, Shape},
    mode,
};

/// The global heap blocks `result` owns: each vector and string with room.
fn owned_blocks(result: &BorrowckResult) -> u64 {
    let text = |it: &String| u64::from(it.capacity() > 0);
    let room = |capacity: usize| u64::from(capacity > 0);
    let labels = |it: &Vec<(PartSpan, String)>| {
        room(it.capacity()) + it.iter().map(|(_, it)| text(it)).sum::<u64>()
    };
    room(result.diagnostics.capacity())
        + result.skipped.as_ref().map_or(0, text)
        + result
            .diagnostics
            .iter()
            .map(|it| {
                text(&it.message)
                    + labels(&it.labels)
                    + room(it.children.capacity())
                    + it.children
                        .iter()
                        .map(|it| text(&it.message) + room(it.spans.capacity()) + labels(&it.labels))
                        .sum::<u64>()
                    + room(it.suggestions.capacity())
                    + it.suggestions.iter().map(|it| text(&it.message) + labels(&it.parts)).sum::<u64>()
            })
            .sum::<u64>()
}

/// The fixture's functions, by name, in the order the modules declare them.
pub(super) fn functions(db: &Db) -> Vec<(String, DefWithBodyId)> {
    let mut found = Vec::new();
    for &krate in all_crates(db).iter() {
        for (_, module) in crate_def_map(db, krate).modules(db) {
            for def in module.scope(db).declarations() {
                if let ModuleDefId::FunctionId(it) = def {
                    let name = FunctionSignature::of(db, it).name.as_str().to_owned();
                    found.push((name, it.into()));
                }
            }
        }
    }
    found
}

/// A typical crate's bodies, which pass: field and enum places, a closure that borrows a
/// capture mutably, a loop that breaks, a value with drop glue moved on one path only.
pub(super) const TYPICAL: &str = r#"
//- minicore: sized, copy, clone, option, drop, fn
struct Pair { a: u32, b: Option<u32> }
struct Bag(u32);
impl Drop for Bag { fn drop(&mut self) {} }
fn keep(_: Bag) {}
fn sum(pair: &Pair) -> u32 { match pair.b { Some(b) => pair.a + b, None => pair.a } }
fn grow(pair: &mut Pair, by: u32) { pair.a += by; if let Some(b) = &mut pair.b { *b += by; } }
fn closures(mut pair: Pair) -> u32 {
    let mut add = |by| grow(&mut pair, by);
    add(1);
    add(2);
    sum(&pair)
}
fn loops(limit: u32, bag: Bag) -> u32 {
    let mut total = 0;
    let mut at = 0;
    while at < limit {
        if at % 3 == 0 { total += at; } else if total > 100 { break; }
        at += 1;
    }
    if total > 10 { keep(bag); }
    total
}
"#;

pub(super) const ERRORS: &str = r#"
//- minicore: sized, copy, clone, option
struct Text;
fn take(_: Text) {}
fn moved_twice() { let text = Text; take(text); take(text); }
fn two_mut() { let mut n = 0; let a = &mut n; let b = &mut n; *a += 1; *b += 1; }
fn unused_mut() { let mut n = 1; let _ = n; }
"#;

/// A function `name` of `segments` segments, each a few locals, a borrow, a value with drop
/// glue moved on one path, a branch and a loop: about 10 locals and 12 blocks a segment, so
/// the move paths and locals run past the dense rows' 1 024 bits.
pub(super) fn large_function(out: &mut String, name: &str, segments: usize) {
    writeln!(out, "fn {name}(flag: bool, n: u32) -> u32 {{").unwrap();
    writeln!(out, "    let mut total = 0;").unwrap();
    for at in 0..segments {
        writeln!(
            out,
            "    let mut v{at} = n + {at};
    let b{at} = Bag({at});
    if flag {{ v{at} += 1; }} else {{ total += v{at}; }}
    let r{at} = &mut v{at};
    *r{at} += 1;
    if v{at} > {at} {{ keep(b{at}); }}
    while v{at} > 3 {{ v{at} -= 2; }}
    total += v{at};"
        )
        .unwrap();
    }
    writeln!(out, "    total\n}}").unwrap();
}

/// The typical bodies and two large functions after them.
fn large_fixture() -> String {
    let mut out = String::from(TYPICAL);
    large_function(&mut out, "largest", 400);
    large_function(&mut out, "large", 250);
    out
}

/// Checks every function of `fixture` once to warm the frame and the memos it reads, then
/// again, measured, those named in `first` first in that order: each allocates its result's
/// own blocks and nothing else, so a clean body allocates nothing, whichever body the frame
/// served before it.
fn allocates_its_result_alone(fixture: &str, first: &[&str], errors: bool) {
    let fx = fixture::db(fixture);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let mut defs = functions(&db);
        assert!(defs.len() >= 3, "{}", defs.len());
        defs.sort_by_key(|(name, _)| first.iter().position(|it| it == name).unwrap_or(first.len()));
        for name in first {
            assert!(defs.iter().any(|(it, _)| it == name), "no function `{name}`");
        }
        for (_, def) in &defs {
            borrowck(&db, *def);
        }
        let mut reported = 0;
        for (name, def) in &defs {
            let (result, used) = measure(|| borrowck_in_mode(&db, *def, mode()));
            assert_eq!(result, *borrowck(&db, *def), "{name}");
            assert!(result.skipped.is_none() && result.not_checked.is_none(), "{name}: {result:?}");
            assert_eq!(
                used.allocations,
                owned_blocks(&result),
                "{name}: {} bytes, {result:?}",
                used.bytes
            );
            if !errors {
                assert_eq!(used.allocations, 0, "{name}: {} bytes", used.bytes);
            }
            reported += result.diagnostics.len();
        }
        assert_eq!(reported > 0, errors, "{reported}");
    });
}

#[test]
fn a_typical_clean_body_allocates_nothing() {
    allocates_its_result_alone(TYPICAL, &[], false);
}

#[test]
fn a_large_clean_body_and_smaller_ones_after_it_allocate_nothing() {
    allocates_its_result_alone(&large_fixture(), &["largest", "large"], false);
}

#[test]
fn a_body_with_errors_allocates_its_diagnostics_alone() {
    allocates_its_result_alone(ERRORS, &[], true);
}

#[test]
fn asking_again_allocates_nothing() {
    for fixture in [TYPICAL, ERRORS] {
        let fx = fixture::db(fixture);
        let _ = fx.only_file();
        let db = fx.into_db();
        hir_ty::attach_db(&db, || {
            for (name, def) in functions(&db) {
                let first: *const BorrowckResult = borrowck(&db, def);
                let (again, used) = measure(|| borrowck(&db, def) as *const BorrowckResult);
                assert_eq!(again, first, "{name}");
                assert_eq!(used.allocations, 0, "{name}");
            }
        });
    }
}

#[test]
fn an_edit_to_another_file_allocates_nothing_to_answer_again() {
    let fx = fixture::db(
        r#"
//- /main.rs
mod other;
struct Text;
fn take(_: Text) {}
fn moved_twice() { let text = Text; take(text); take(text); }
fn fine(n: &mut u32) { *n += 1; }
//- /other.rs
pub fn elsewhere() -> u32 { 1 }
"#,
    );
    let files = fx.files().to_vec();
    let mut db = fx.into_db();
    let other = files[1].file_id(&db);
    let defs = hir_ty::attach_db(&db, || {
        let defs: Vec<_> =
            // alloc: the test's setup, before anything is measured.
            functions(&db).into_iter().filter(|(name, _)| name != "elsewhere").collect();
        for (_, def) in &defs {
            borrowck(&db, *def);
        }
        defs
    });
    db.set_file_text(other, "pub fn elsewhere() -> u32 { 2 }\n");
    hir_ty::attach_db(&db, || {
        for (name, def) in &defs {
            let (_, used) = measure(|| borrowck(&db, *def));
            assert_eq!(used.allocations, 0, "{name}");
        }
    });
}

#[test]
fn a_passing_gate_allocates_nothing() {
    let fx = fixture::db(TYPICAL);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        for (name, def) in functions(&db) {
            assert!(crate::gate::checked(&db, def).is_ok(), "{name}: the typical fixture passes");
            let (passed, used) = measure(|| crate::gate::checked(&db, def).is_ok());
            assert!(passed, "{name}");
            assert_eq!(used.allocations, 0, "{name}");
        }
    });
}

/// The fixpoint on large generated bodies (nested loops; every shape strung together), as
/// each of the six analyses drives it, run once to warm the frame and then measured.
#[test]
fn composing_solving_and_reading_a_large_body_allocates_nothing() {
    let mut rng = SplitMix64::new(0xbb67_ae85_84ca_a73b);
    for shape in [Shape::Loops, Shape::Mixed] {
        frame::task(Tag::Mir, |frame| {
            let case = differential::large_case(frame, &mut rng, shape);
            let row = frame.alloc_slice_fill_with(case.words(), |_| 0);
            let want = differential::solve(frame, &case, row);
            let got = assert_at_most(0, &format!("solving a {shape:?} body"), || {
                differential::solve(frame, &case, row)
            });
            assert_eq!(got, want, "{shape:?}");
        });
    }
}
