//! A callee the survey LEFT OUT prints the survey's own cause, wherever the walk meets it.
//!
//! The fault this guards (2026-11-16, port27/front2/list-AJ.txt lines 447 and 585): the same
//! item, core's provided `fmt::Write::write_fmt` (0x2980e808e42ac8c9d1437b12c07a9a56), was
//! filed twice as "left out: ..: check: Diags: E0599" (reached by a direct call, which asks
//! run.rs `callee_refusal`) and seven times as "callee ..: neither a tabled body nor an outside
//! symbol (never reached by the walk)" (reached as a method word of `String`'s `dyn fmt::Write`
//! vtable, made by `spec_to_string`'s `Formatter::new(&mut buf)`, whose site built
//! `RunError::Callee` itself). The vtable site, and the three instance sites of `build_from`,
//! now go through `callee_refusal` too, so a left-out callee is refused with its cause.
//!
//! The rules the program below rests on (the Rust Reference):
//! - "Functions": "the type of the block [the function body] must be coercible to the declared
//!   return type"; `true` is a `bool` and no coercion makes it an `i32`, so the provided `m`
//!   is refused by the check, and the survey leaves it out with that cause.
//! - "Traits", "Trait and trait object" items: an impl that does not define a provided method
//!   uses the trait's definition; "Trait objects": a call through `&dyn Tr` calls the method of
//!   the concrete type's impl, read from the vtable. So `S`'s vtable for `Tr` names the
//!   provided `m`, and `main`'s coercion `&S` to `&dyn Tr` needs that vtable.
//! - "Trait objects" again: "the vtable pointer ... is used to look up the method at run time":
//!   `call`'s `t.m()` is a dynamic call, the same code for every type behind `&dyn Tr`; it does
//!   not name `S`'s `m` (or the provided one) at all, so `call` needs no body of `m`.
//!
//! The rule of the rows (walk-AJ P13-2): one row per refused FUNCTION (the survey's unit is the
//! fn: run.rs `filed` pushes `(function, class)`), in walk order (the entry first, then the
//! seeds by key: run.rs `build_from`), each naming a left-out callee once. The 17:50 run
//! (BUILD-STATUS-FRONT.md) showed two rows naming `m`'s cause, `main`'s and `call`'s: two
//! DIFFERENT functions, so not a double filing of one site. `main`'s row is right (the vtable
//! site, the fix this file guards); `call`'s is not: by reading, a row of `call` naming `m`
//! can only come from `build_from`'s refs loop (`fn_index` found `m`, no body,
//! `callee_refusal`), i.e. `call`'s built refs hold `Tr::m`: blaze-build walk.rs `dyn_slot`
//! returned `None` for `t.m()` and the call was built as a DIRECT call of the provided `m`
//! (wrong code where an impl overrides `m`). That is
//! [`a_dynamic_call_needs_no_body_of_the_method`]'s to show, kept apart from the vtable fix.
//!
//! It needs no toolchain: the program is `#![no_core]` (as tests/front2_survey.rs).
#![feature(allocator_api)]

use std::alloc::Global;

use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front2::program::Front2Program;
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::{NewFront, build_survey};

/// `main` unsizes `&S` to `&dyn Tr` and calls `m` only through it: the ONLY path from `main`
/// to `m` is the vtable's method word, so without the fix the refusal is the vtable site's
/// "neither a tabled body". The lang items are modeled on the pinned library's own
/// (library/core/src/marker.rs, ops/unsize.rs, ops/deref.rs), as the ladder's twin prelude.
const PROGRAM: &str = r#"#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = "pointee_sized"]
trait PointeeSized {}

#[lang = "meta_sized"]
trait MetaSized: PointeeSized {}

#[lang = "sized"]
trait Sized: MetaSized {}

#[lang = "legacy_receiver"]
trait LegacyReceiver: PointeeSized {}

impl<T: PointeeSized> LegacyReceiver for &T {}

#[lang = "copy"]
trait Copy {}

#[lang = "unsize"]
trait Unsize<T: PointeeSized>: PointeeSized {}

#[lang = "coerce_unsized"]
trait CoerceUnsized<T: PointeeSized> {}

impl<'a, 'b: 'a, T: PointeeSized + Unsize<U>, U: PointeeSized> CoerceUnsized<&'a U> for &'b T {}

#[lang = "dispatch_from_dyn"]
trait DispatchFromDyn<T> {}

impl<'a, T: PointeeSized + Unsize<U>, U: PointeeSized> DispatchFromDyn<&'a U> for &'a T {}

trait Tr {
    fn m(&self) -> i32 {
        true
    }
}

struct S;

impl Tr for S {}

fn call(t: &dyn Tr) -> i32 {
    t.m()
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let s = S;
    call(&s)
}
"#;

/// Whether the survey's fn path `f` is the crate-root fn `name` (run.rs `Named`'s `path`).
fn is_fn(f: &str, name: &str) -> bool {
    f == name || f.ends_with(&*format!("::{name}"))
}

/// The survey of [`PROGRAM`]: `m`'s left-out cause and the refused rows `(fn, class)` in the
/// order the walk filed them.
fn survey_rows() -> (String, Vec<(String, String)>) {
    let files = [SourceFile { path: "src/main.rs", text: PROGRAM }];
    let tc = Toolchain::none(blaze_own::Alloc::global());
    let new = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind).unwrap_or_else(|e| panic!("the program's texts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the survey's frame");
    let mut left_out = Vec::new_in(Global);
    let mut program = Front2Program::survey(&names, &frame, &files, &mut left_out, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("BUILDS: no survey: {e}"));
    let left: Vec<(String, String)> = left_out.iter().map(|(f, class)| (f.to_string(), class.to_string())).collect();
    println!("LEFT OUT: {left:?}");
    // The one left-out fn is the provided `m`, with the check's cause.
    let of_m: Vec<&(String, String)> = left.iter().filter(|(f, _)| is_fn(f, "m")).collect();
    assert_eq!(of_m.len(), 1, "front2 leaves out the provided `m` (its body is a `bool` where an `i32` is declared): {left:?}");
    let cause = of_m[0].1.clone();
    let mut refused = Vec::new_in(Global);
    let counts = build_survey(&mut program, &mut refused, blaze_build::OverflowChecks::On, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("BUILDS: the survey stopped: {e}"));
    let rows: Vec<(String, String)> = refused.iter().map(|(f, class)| (f.to_string(), class.to_string())).collect();
    let shown: Vec<String> = rows.iter().map(|(f, class)| format!("{f}: {class}")).collect();
    println!("BUILDS: {} of {} instances ({} refused: {})", counts.built, counts.tried, rows.len(), shown.join("; "));
    (cause, rows)
}

/// The rows of `rows` naming `cause` as a left-out callee's (the filed class is cut at 240
/// characters: the cause's first 40 are inside it).
fn naming<'r>(rows: &'r [(String, String)], cause: &str) -> Vec<&'r (String, String)> {
    let head: String = cause.chars().take(40).collect();
    rows.iter().filter(|(_, class)| class.contains("left out: ") && class.contains(&*head)).collect()
}

/// `main`, whose `dyn Tr` vtable names the left-out `m`, is refused with ONE row that starts
/// "left out:" and carries `m`'s cause, never "neither a tabled body nor an outside symbol";
/// it is the walk's first row (the entry is walked first); and no fn has two rows naming `m`
/// (one row per refused function: a second row of one fn would be a double filing).
///
/// Prints: `BUILDS: .. (.. refused: main: left out: ..)` and
/// `test a_left_out_callee_prints_its_cause ... ok`
#[test]
fn a_left_out_callee_prints_its_cause() {
    let (cause, rows) = survey_rows();
    assert!(
        rows.iter().all(|(_, class)| !class.contains("neither a tabled body")),
        "a left-out callee is refused with its cause, not as one the walk never reached: {rows:?}"
    );
    let of_main: Vec<&(String, String)> = rows.iter().filter(|(f, _)| is_fn(f, "main")).collect();
    assert_eq!(of_main.len(), 1, "one row for `main`, whose vtable names `m`: {rows:?}");
    assert!(of_main[0].1.starts_with("left out: "), "`main`'s row starts \"left out:\": {rows:?}");
    assert_eq!(naming(&rows, &cause).iter().filter(|(f, _)| is_fn(f, "main")).count(), 1, "`main`'s row names `m`'s cause ({cause}): {rows:?}");
    assert!(rows.first().is_some_and(|(f, _)| is_fn(f, "main")), "the entry's row is filed first (walk order): {rows:?}");
    let named = naming(&rows, &cause);
    for (k, (f, _)) in named.iter().enumerate() {
        assert!(!named[..k].iter().any(|(g, _)| g == f), "`{f}` has two rows naming `m`'s cause (one row per refused fn): {rows:?}");
    }
}

/// `call`'s `t.m()` is a dynamic call (the Reference, "Trait objects": the method is looked up
/// in the vtable at run time), so `call` is built whatever `S`'s `m` is: it is not refused, and
/// the only row naming `m`'s cause is `main`'s. A row for `call` means blaze-build walk.rs
/// `dyn_slot` gave `None` for `t.m()` (no `Self` argument, a `Self` not `Dynamic`, no
/// principal, or no slot for the callee in `Shape::of`) and the call was built as a direct
/// call of the provided `m`, which run.rs `build_from`'s refs loop then filed.
///
/// Prints: `BUILDS: .. (1 refused: main: left out: ..)` and
/// `test a_dynamic_call_needs_no_body_of_the_method ... ok`
#[test]
fn a_dynamic_call_needs_no_body_of_the_method() {
    let (cause, rows) = survey_rows();
    assert!(
        !rows.iter().any(|(f, _)| is_fn(f, "call")),
        "`call` dispatches `m` through the vtable and needs no body of it; a row for `call` is `t.m()` built as a direct call (blaze-build walk.rs `dyn_slot` gave `None`): {rows:?}"
    );
    assert_eq!(naming(&rows, &cause).len(), 1, "only `main`'s row names `m`'s cause ({cause}): {rows:?}");
}
