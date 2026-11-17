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
//! Needs no toolchain: the program is `#![no_core]` (setup as tests/front2_survey.rs).

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

/// The survey of [`PROGRAM`] refuses nothing: the closure's drop glue is built with no `Drop`
/// lookup for the closure type, and `G`'s impl is found by `G`'s head.
///
/// Prints: `BUILDS: n of n instances (0 refused: )` and
/// `test a_closure_value_has_no_drop_impl_lookup ... ok`.
/// Without the fix: one refusal "selecting a `Drop` impl: .. BodyTypeInRecord(Closure(".
#[test]
fn a_closure_value_has_no_drop_impl_lookup() {
    let files = [SourceFile { path: "src/main.rs", text: PROGRAM }];
    let tc = Toolchain::none();
    let new = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = ProgramTexts::new(&files, "src/main.rs", new, &tc).unwrap_or_else(|e| panic!("the program's texts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the survey's frame");
    let mut left_out = Vec::new_in(Global);
    let mut program = Front2Program::survey(&names, &frame, &files, &mut left_out).unwrap_or_else(|e| panic!("BUILDS: no survey: {e}"));
    let left: Vec<String> = left_out.iter().map(|(f, class)| format!("{f}: {class}")).collect();
    assert!(left.is_empty(), "front2 tables every fn of the program; left out: {left:?}");
    let mut refused = Vec::new_in(Global);
    let counts = build_survey(&mut program, &mut refused, blaze_build::OverflowChecks::On).unwrap_or_else(|e| panic!("BUILDS: the survey stopped: {e}"));
    let rows: Vec<String> = refused.iter().map(|(f, class)| format!("{f}: {class}")).collect();
    println!("BUILDS: {} of {} instances ({} refused: {})", counts.built, counts.tried, rows.len(), rows.join("; "));
    assert!(rows.is_empty(), "a closure's drop glue asks no `Drop` impl of the closure: {rows:?}");
    assert!(counts.tried > 0, "the survey built `main` at least");
    assert_eq!(counts.built, counts.tried, "every instance built: {rows:?}");
}
