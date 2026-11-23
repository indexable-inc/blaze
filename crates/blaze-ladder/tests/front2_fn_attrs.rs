//! A fn item's typing attributes are answered from its head (front2/fn_attrs.rs, the world's
//! `attrs` for a fn key), so a body holding a borrowed call `&f()` checks: rung
//! x0-drop-11-zst-temporary's regression (`touch(&make())` refused "NotAHead(.., \"a fn (asked
//! for as an ADT)\")" after blaze-typeck rules/promote.rs began asking `Attrs` of the callee).
//!
//! The Reference, "Function item types": "When referred to, a function item, or the
//! constructor of a tuple-like struct or enum variant, yields a zero-sized value of its function
//! item type." The Reference, "Destructors", "Constant promotion": "Promotion of a value
//! expression to a `'static` slot occurs when the expression could be written in a constant and
//! borrowed, .. and the resulting value does not contain interior mutability or destructors."
//! The library marks the calls that may be promoted `#[rustc_promotable]`
//! (library/core/src/task/wake.rs, `RawWakerVTable::new`).
//!
//! `run` borrows four temporaries:
//! - `&make`: a zero-sized temporary of the fn item type `fn() -> Guard {make}`: a path to a fn
//!   item, with no drop glue: promoted;
//! - `&seven()`: a call of a `#[rustc_promotable]` const fn giving an `i32`: promoted;
//! - `&eight()`: the same call shape without the attribute: not promoted;
//! - `touch(&make())`: a `Guard`, which has a destructor (and `make` is not promotable): not
//!   promoted, dropped at the statement's end.
//!
//! What it asserts: `run` checks with no diagnostic and `Checked::promoted` holds exactly 2.
//! Before the fix `run` did not check (`CheckFail::Head { .., NotAHead(.., "a fn (asked for as
//! an ADT)") }`); a fix answering every fn "no attributes" gives 1 (only `&make`); one answering
//! every fn promotable gives 3 (`&eight()` too).
//!
//! Line it prints: `fn attrs: run checked, promoted 2`.
//!
//! It needs no toolchain: the program is `#![no_core]` (as tests/front2_head_abi.rs).
//! NOT RUN by the writer (a write-only lane).
#![feature(allocator_api)]

use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{BuildOrder, Records, check_body};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::NewFront;
use blaze_lower::scoped::Prepares;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

/// The lang items are modeled on the pinned library's own (library/core/src/marker.rs,
/// ops/drop.rs), as rung x0-drop-11-zst-temporary's.
const PROGRAM: &str = r#"#![feature(no_core, lang_items, rustc_attrs)]
#![no_core]
#![no_main]

#[lang = "pointee_sized"]
trait PointeeSized {}

#[lang = "meta_sized"]
trait MetaSized: PointeeSized {}

#[lang = "sized"]
trait Sized: MetaSized {}

#[lang = "copy"]
trait Copy {}

#[lang = "drop"]
trait Drop {
    fn drop(&mut self);
}

struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {}
}

fn make() -> Guard {
    Guard
}

fn touch(_: &Guard) -> i32 {
    0
}

#[rustc_promotable]
const fn seven() -> i32 {
    7
}

const fn eight() -> i32 {
    8
}

fn run() -> i32 {
    let _f = &make;
    let _s: &i32 = &seven();
    let _e: &i32 = &eight();
    touch(&make())
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    run()
}
"#;

#[test]
fn a_borrowed_call_of_a_fn_item_checks_and_reads_its_attributes() {
    let heap = blaze_own::testing::heap();
    let files = [SourceFile { path: "src/main.rs", text: PROGRAM }];
    let tc = Toolchain::none(blaze_own::Alloc::global());
    let new = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind).unwrap_or_else(|e| panic!("the program's texts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
    let mut front = Front2::new(&names, &frame, &files, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let run = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &["run"] }, heap.alloc())
        .ok()
        .and_then(|f| f.whole(1))
        .map(|d| d.key)
        .unwrap_or_else(|| panic!("`run` from the crate root"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    store.ensure(&names, &mut front.keys, Want::Fn(run)).unwrap_or_else(|e| panic!("run's head: {e:?}"));
    let body = lower_fn(&names, prepared, &mut front.keys, &mut store, run).unwrap_or_else(|e| panic!("run lowers: {e}"));
    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
    let view = body.lowered.view();
    let spell = Spelling::of(&view).expect("run's spelling");
    let scope = body.scope(&spell);
    let checked = check_body(&mut records, &body, &scope).unwrap_or_else(|e| panic!("run did not check: {e:?}"));
    assert!(checked.diags.is_empty(), "run checks with no diagnostic: {:?}", checked.diags);
    let line = format!("fn attrs: run checked, promoted {}", checked.promoted.len());
    println!("{line}");
    assert_eq!(line, "fn attrs: run checked, promoted 2", "`&make` and `&seven()` promoted; `&eight()` and `&make()` not");
}
