//! A closure value's drop glue asks for no `Drop` impl of the closure itself (walkAJ-P3).
//!
//! The fault this guards (2026-11-16, port27/front2/walk-AJ-briefs.md "P3", 15 refusals in the
//! walk of the owner's hello world: `call_once_force`, `each`, `drop`): run.rs `drop_impl`
//! asked the checker's solver `selects(drop, ty)` for every aggregate laid out, a closure type
//! included; selection imports its arguments, and importing a closure type needs its owner's
//! checked record, which the run path's world does not hold: "selecting a `Drop` impl:
//! resolving an instance: blaze-typeck: malformed input: BodyTypeInRecord(Closure(..))".
//!
//! The rule: only a struct, enum or union can implement `Drop` (E0120: "the Drop trait may
//! only be implemented for structs, enums, and unions"); the Reference, "Destructors": "If T
//! implements Drop ... then ... the destructor of T", and a closure drops only its captures;
//! a `Drop` impl covers every instance of its ADT (E0366/E0367). So `drop_impl` answers by the
//! ground type's head: a closure has none, without the solver; an ADT's is found in its own
//! bucket (blaze-typeck drop_impl.rs `adt_drop_impl`), its arguments never imported.
//! Needs no toolchain: the programs are `#![no_core]` (setup as tests/front2_survey.rs).
//!
//! WHICH TEST IS THE CONTROL (walkAJ-P3-3, rule 23). blaze-build asks `Adts::drop_impl` only
//! of an ADT (walk.rs and drop_facts.rs, inside their `PNode::Adt` arms), so [`PROGRAM`]'s bare
//! closure never reaches it; and a closure inside an ADT is imported since the front2 world
//! answers any checked body's closures (front2/foreign_closures.rs). What the head-first
//! answer alone decides is an ADT over an async future: selection imports every argument, and
//! import refuses a future (blaze-typeck instance.rs: `Asked` inherits `future_of`, which returns
//! `Malformed::InstanceImportFuture { block }`; a `Coroutine` is `BodyTypeInRecord`). [`ASYNC_MAIN`] holds one; its test
//! fails with "selecting a `Drop` impl:" when run.rs `drop_impl` selects instead.

#![feature(allocator_api)]

use std::alloc::Global;

use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front2::program::Front2Program;
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{NewFront, build_survey};

/// `main` makes a `move` closure capturing a `G`, which has a `Drop` impl, and never calls it:
/// at `main`'s end the closure is dropped, so its drop glue is built, and that glue drops the
/// captured `G` through `G`'s impl. The lang items are core's (library/core/src/ops/drop.rs
/// `#[lang = "drop"]`, ptr/mod.rs `#[lang = "drop_in_place"]`, ops/function.rs `fn_once`,
/// `fn_mut`, `fn`), as in the ladder's no_core closure rungs (x0-clo-09's prelude).
const PROGRAM: &str = "#![feature(no_core, lang_items, unboxed_closures, rustc_attrs)]
#![no_core]
#![no_main]
#![allow(internal_features)]

#[lang = \"pointee_sized\"]
trait PointeeSized {}

#[lang = \"meta_sized\"]
trait MetaSized: PointeeSized {}

#[lang = \"sized\"]
trait Sized: MetaSized {}

#[lang = \"copy\"]
trait Copy {}

#[lang = \"drop\"]
trait Drop {
    fn drop(&mut self);
}

#[lang = \"drop_in_place\"]
unsafe fn drop_in_place<T: PointeeSized>(to_drop: *mut T) {
    unsafe { drop_in_place(to_drop) }
}

#[lang = \"tuple_trait\"]
trait Tuple {}

#[lang = \"fn_once\"]
#[rustc_paren_sugar]
trait FnOnce<Args: Tuple> {
    #[lang = \"fn_once_output\"]
    type Output;
    extern \"rust-call\" fn call_once(self, args: Args) -> Self::Output;
}

#[lang = \"fn_mut\"]
#[rustc_paren_sugar]
trait FnMut<Args: Tuple>: FnOnce<Args> {
    extern \"rust-call\" fn call_mut(&mut self, args: Args) -> Self::Output;
}

#[lang = \"fn\"]
#[rustc_paren_sugar]
trait Fn<Args: Tuple>: FnMut<Args> {
    extern \"rust-call\" fn call(&self, args: Args) -> Self::Output;
}

struct G(i32);

impl Drop for G {
    fn drop(&mut self) {}
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let g = G(1);
    let _c = move || {
        let _x = &g;
    };
    0
}
";

/// `main` holds an unpolled `async fn` future in `Held`, a generic struct with no `Drop` impl,
/// and drops it at its end: `Held<{future of ready}>` is laid out, so its `Drop` impl is asked
/// (walk.rs `drop_fn`) and whether it needs drop (drop_facts.rs `needs_drop`, whose ADT arm asks
/// the same before reading the fields). The Reference, "Destructors": a value is dropped at
/// the end of its scope by its type's destructor; "Async blocks": an async block's future that
/// was never polled drops what it captured when it is dropped. The async lang items are the
/// rung x0-async-01's (its prelude.rs and future.rs: core's `future_trait`, `Poll`, `Context`,
/// `pin`, `coroutine` items), included as written.
const ASYNC_MAIN: &str = "#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures, arbitrary_self_types)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod future;
mod prelude;
use future::*;
use prelude::*;

struct G(u32);

impl Drop for G {
    fn drop(&mut self) {}
}

/// Holds its value; no `Drop` impl of its own.
struct Held<F>(F);

async fn ready(g: G) -> u32 {
    g.0
}

/// Holds `f` in `Held` and drops it at its end: `Held<F>` is laid out at `F` = the future.
fn keep<F>(f: F) -> i32 {
    let _h = Held(f);
    0
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    keep(ready(G(1)))
}
";

/// The rung x0-async-01's no_core prelude and future items (crates/blaze-cli/tests/ladder).
const ASYNC_PRELUDE: &str = include_str!("../../blaze-cli/tests/ladder/front2/x0-async-01/src/prelude.rs");
const ASYNC_FUTURE: &str = include_str!("../../blaze-cli/tests/ladder/front2/x0-async-01/src/future.rs");

/// Surveys the no_core program `files` (its root `src/main.rs`): the refused rows and the
/// counts, after asserting front2 tabled every fn.
fn survey(files: &[SourceFile<'_>]) -> (Vec<String>, blaze_ladder::run::SurveyCounts) {
    let tc = Toolchain::none(blaze_own::Alloc::global());
    let new = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = ProgramTexts::new(files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind).unwrap_or_else(|e| panic!("the program's texts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the survey's frame");
    let mut left_out = Vec::new_in(Global);
    let mut program = Front2Program::survey(&names, &frame, files, &mut left_out, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("BUILDS: no survey: {e}"));
    let left: Vec<String> = left_out.iter().map(|(f, class)| format!("{f}: {class}")).collect();
    assert!(left.is_empty(), "front2 tables every fn of the program; left out: {left:?}");
    let mut refused = Vec::new_in(Global);
    let counts = build_survey(&mut program, &mut refused, blaze_build::OverflowChecks::On, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("BUILDS: the survey stopped: {e}"));
    let rows: Vec<String> = refused.iter().map(|(f, class)| format!("{f}: {class}")).collect();
    println!("BUILDS: {} of {} instances ({} refused: {})", counts.built, counts.tried, rows.len(), rows.join("; "));
    (rows, counts)
}

/// The survey of [`PROGRAM`] refuses nothing: the closure's drop glue is built with no `Drop`
/// lookup for the closure type, and `G`'s impl is found by `G`'s head. NOT edit 5's control
/// (see the module doc): it passes with run.rs `drop_impl` selecting too.
///
/// Prints: `BUILDS: n of n instances (0 refused: )` and
/// `test a_closure_value_has_no_drop_impl_lookup ... ok`.
#[test]
fn a_closure_value_has_no_drop_impl_lookup() {
    let files = [SourceFile { path: "src/main.rs", text: PROGRAM }];
    let (rows, counts) = survey(&files);
    assert!(rows.is_empty(), "a closure's drop glue asks no `Drop` impl of the closure: {rows:?}");
    assert!(counts.tried > 0, "the survey built `main` at least");
    assert_eq!(counts.built, counts.tried, "every instance built: {rows:?}");
}

/// The survey of [`ASYNC_MAIN`] refuses nothing: `Held<{future}>`'s `Drop` impl is looked up
/// in `Held`'s own bucket (none), the future never imported; the future's own drop is its
/// coroutine's (drop_facts.rs: `PNode::Future` needs drop, no impl asked).
///
/// Prints: `BUILDS: n of n instances (0 refused: )` and
/// `test an_adt_over_a_future_asks_its_drop_impl_by_its_head ... ok`.
/// Without run.rs `drop_impl`'s head-first answer (selecting `Drop` at `Held<{future}>`): a
/// refusal "selecting a `Drop` impl: .." from importing the future (blaze-typeck instance.rs
/// `import`: `Malformed::InstanceImportFuture { block }` for `PNode::Future`, `BodyTypeInRecord(Coroutine(` for
/// `PNode::Coroutine`).
#[test]
fn an_adt_over_a_future_asks_its_drop_impl_by_its_head() {
    let files = [
        SourceFile { path: "src/main.rs", text: ASYNC_MAIN },
        SourceFile { path: "src/prelude.rs", text: ASYNC_PRELUDE },
        SourceFile { path: "src/future.rs", text: ASYNC_FUTURE },
    ];
    let (rows, counts) = survey(&files);
    assert!(rows.is_empty(), "an ADT over a future asks its `Drop` impl by its head, importing nothing: {rows:?}");
    assert!(counts.tried > 0, "the survey built `main` at least");
    assert_eq!(counts.built, counts.tried, "every instance built: {rows:?}");
}
