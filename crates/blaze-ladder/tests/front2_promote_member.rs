//! STD-CENSUS-1 class 1 (48 of 57 std rungs stop there), reduced: std's `panic_with_hook`
//! (library/std/src/panicking.rs, nightly-2026-10-27, line 773) writes
//! ```text
//!         Hook::Default => {
//!             default_hook(&PanicHookInfo::new(
//!                 location,
//!                 payload.get(),
//!                 can_unwind,
//!                 force_no_backtrace,
//!             ));
//!         }
//! ```
//! a shared borrow of a call of an INHERENT IMPL's fn (library/std/src/panic.rs line 49:
//! `impl<'a> PanicHookInfo<'a> { #[inline] pub(crate) fn new(..) -> Self`). blaze-typeck's
//! constant promotion (rules/promote.rs `promotable_callee`) asks the callee's `attrs` to read
//! `#[rustc_promotable]`; the callee's key is a MEMBER key (front2/keys.rs `member_key`), and
//! the world answered `attrs` for ADTs only: "the checker key 0x5ea6563b3105530056105aba956f02c3
//! .. is in no row of the front end's key map, and its crate's records hold no item under it
//! (gap Fill/UnknownKey)" (port27/tempo-run/std-census-1-s.txt, rung s0-fmt-01-integers).
//!
//! The Reference, "Destructors", "Constant promotion": "Promotion of a value expression to a
//! `'static` slot occurs when the expression could be written in a constant and borrowed, and
//! that borrow could be dereferenced where the expression was originally written, without
//! changing the runtime behavior." The library marks the calls that may be promoted, and marks
//! them on INHERENT IMPL fns (library/core/src/task/wake.rs lines 52-56 and 197-207:
//! `impl RawWaker { #[rustc_promotable] .. pub const fn new(..) }`, `impl RawWakerVTable {
//! #[rustc_promotable] .. pub const fn new(..) }`), so a member fn's attribute is read too:
//! - `&Info::seven()`: a call of a member const fn carrying `#[rustc_promotable]`, giving an
//!   `i32`: promoted;
//! - `&Info::eight()`: the same shape without the attribute: not promoted;
//! - `show(&Info::new(a))`: panic_with_hook's shape (a member fn, a local argument): not
//!   promoted, and it must CHECK.
//!
//! What it asserts: `hook` checks with no diagnostic and `Checked::promoted` holds exactly 1.
//! Before the fix (wiring x0-drop-11-1's world `attrs`) `hook` does not check: the panic line is
//! `CheckFail::Gap` naming `Fill(UnknownKey(..))` of a member key, the census's class 1. A fix
//! answering every fn "no attributes" gives 0; one answering every fn promotable gives 2
//! (`&Info::eight()` too); one that reads a member's attributes from a text starting after them
//! gives 0.
//!
//! Line it prints: `promote member: hook checked, promoted 1`.
//!
//! It needs no toolchain: the program is `#![no_core]` (as tests/front2_fn_attrs.rs).
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

/// The lang items are modeled on the pinned library's own (library/core/src/marker.rs), as
/// tests/front2_fn_attrs.rs's.
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

struct Info {
    a: i32,
}

impl Info {
    #[inline]
    fn new(a: i32) -> Info {
        Info { a: a }
    }

    /// A doc line first, as the library writes it above the attribute.
    #[rustc_promotable]
    const fn seven() -> i32 {
        7
    }

    const fn eight() -> i32 {
        8
    }
}

fn show(_: &Info) -> i32 {
    0
}

fn hook(a: i32) -> i32 {
    let _s: &i32 = &Info::seven();
    let _e: &i32 = &Info::eight();
    show(&Info::new(a))
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    hook(1)
}
"#;

#[test]
fn a_borrowed_call_of_a_member_fn_checks_and_reads_its_attributes() {
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
    let hook = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &["hook"] }, heap.alloc())
        .ok()
        .and_then(|f| f.whole(1))
        .map(|d| d.key)
        .unwrap_or_else(|| panic!("`hook` from the crate root"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    store.ensure(&names, &mut front.keys, Want::Fn(hook)).unwrap_or_else(|e| panic!("hook's head: {e:?}"));
    let body = lower_fn(&names, prepared, &mut front.keys, &mut store, hook).unwrap_or_else(|e| panic!("hook lowers: {e}"));
    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
    let view = body.lowered.view();
    let spell = Spelling::of(&view).expect("hook's spelling");
    let scope = body.scope(&spell);
    let checked = check_body(&mut records, &body, &scope).unwrap_or_else(|e| panic!("hook did not check (STD-CENSUS-1 class 1 when it names Fill(UnknownKey(..))): {e:?}"));
    assert!(checked.diags.is_empty(), "hook checks with no diagnostic: {:?}", checked.diags);
    let line = format!("promote member: hook checked, promoted {}", checked.promoted.len());
    println!("{line}");
    assert_eq!(line, "promote member: hook checked, promoted 1", "`&Info::seven()` promoted; `&Info::eight()` and `&Info::new(a)` not");
}
