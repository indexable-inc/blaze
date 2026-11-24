//! A promoted borrow's VALUE, made by front2's constant evaluator from the owner's body (lane
//! T04, wave 2311). Before it, no build's `Adts` answered `promoted_value` (run.rs `FrontAdts`
//! held only the trait's default `None`), so every function with a promoted borrow stopped:
//! hello's copy (round 3, tree 4dd8a4485e) had twelve stubs "the promoted borrow of fn 0x.. has
//! no promoted value at this instance in the build's `Adts`" (`promotable_even_clone`,
//! `yield_now`, `<impl Repr>::new_custom`, `remove_file`, ..). Now each instance's needs list
//! every `Checked::promoted` borrow (front2/const_at.rs `ConstNeed::Promoted`), pulled before
//! its walk through `Consts::body_value` with `BodyRoot::Promoted`, which evaluates the
//! borrow's OPERAND from the owner's checked body.
//!
//! The rule: the Reference, "Destructors", "Constant promotion": "Promotion of a value
//! expression to a `'static` slot occurs when the expression could be written in a constant and
//! borrowed"; the slot holds the operand's value, and the builder takes its row's address
//! (blaze-build walk/promote.rs).
//!
//! What it asserts: `pick`'s two promoted borrows evaluate to their operands' bytes (`&7` to
//! `07 00 00 00`, `&(4, 5)` to `04 00 00 00 05 00 00 00`), with no relocation; and an expression
//! that is no promoted borrow is refused by name (`ConstFail::NotPromoted`). A wrong version
//! that evaluated the borrow itself gives an 8-byte pointer with a relocation; one that skipped
//! the check of `Checked::promoted` evaluates the non-promoted expression instead of refusing.
//!
//! Lines it prints (and asserts):
//!   `promoted value: 04 00 00 00 05 00 00 00`
//!   `promoted value: 07 00 00 00`
//!   `not promoted: refused (NotPromoted)`
//!
//! It needs no toolchain: the program is `#![no_core]` (as tests/front2_promote_member.rs).
//! NOT RUN by the writer (a write-only lane).
#![feature(allocator_api)]

use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{BuildOrder, Records, check_body};
use blaze_ladder::front2::const_at::promoted_key;
use blaze_ladder::front2::consts::{BodyRoot, ConstFail, Consts};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::NewFront;
use blaze_lower::scoped::Prepares;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

/// The lang items as tests/front2_promote_member.rs models them on the pinned library's own.
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

fn pick() -> &'static i32 {
    let _pair: &'static (i32, i32) = &(4, 5);
    &7
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    *pick()
}
"#;

#[test]
fn a_promoted_borrow_evaluates_to_its_operand() {
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
    let pick = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &["pick"] }, heap.alloc())
        .ok()
        .and_then(|f| f.whole(1))
        .map(|d| d.key)
        .unwrap_or_else(|| panic!("`pick` from the crate root"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    store.ensure(&names, &mut front.keys, Want::Fn(pick)).unwrap_or_else(|e| panic!("pick's head: {e:?}"));
    let owner = front.keys.item(pick).unwrap_or_else(|_| panic!("pick's checker key"));
    let body = lower_fn(&names, prepared, &mut front.keys, &mut store, pick).unwrap_or_else(|e| panic!("pick lowers: {e}"));
    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
    let promoted: Vec<blaze_typeck::ExprId> = {
        let view = body.lowered.view();
        let spell = Spelling::of(&view).expect("pick's spelling");
        let scope = body.scope(&spell);
        let checked = check_body(&mut records, &body, &scope).unwrap_or_else(|e| panic!("pick did not check: {e:?}"));
        assert!(checked.diags.is_empty(), "pick checks with no diagnostic: {:?}", checked.diags);
        checked.promoted.iter().copied().collect()
    };
    assert_eq!(promoted.len(), 2, "`&(4, 5)` and `&7` are promoted borrows");
    let mut consts = Consts::default();
    let mut lines = Vec::new();
    for &borrow in &promoted {
        let key = promoted_key(owner, borrow, PROGRAM);
        let line = match consts.body_value(&mut records, prepared, owner, BodyRoot::Promoted(borrow), key, blaze_typeck::PTys::EMPTY, &[], None, heap.alloc()) {
            Ok(rec) => match rec.data.allocs.first() {
                Some(root) if root.relocs.is_empty() && rec.data.allocs.len() == 1 => {
                    let bytes: Vec<String> = root.bytes.iter().map(|b| format!("{b:02x}")).collect();
                    format!("promoted value: {}", bytes.join(" "))
                }
                _ => format!("promoted value: a record that is no plain value: {rec:?}"),
            },
            Err(e) => format!("promoted value: refused: {e}"),
        };
        println!("{line}");
        lines.push(line);
    }
    lines.sort();
    assert_eq!(lines, ["promoted value: 04 00 00 00 05 00 00 00", "promoted value: 07 00 00 00"]);
    // An expression id no body holds: never a promoted borrow, refused by name.
    let nowhere = blaze_typeck::ExprId(u32::MAX);
    let refused = match consts.body_value(&mut records, prepared, owner, BodyRoot::Promoted(nowhere), promoted_key(owner, nowhere, PROGRAM), blaze_typeck::PTys::EMPTY, &[], None, heap.alloc()) {
        Err(ConstFail::NotPromoted { .. }) => "not promoted: refused (NotPromoted)".to_string(),
        Err(e) => format!("not promoted: refused otherwise: {e}"),
        Ok(rec) => format!("not promoted: evaluated: {rec:?}"),
    };
    println!("{refused}");
    assert_eq!(refused, "not promoted: refused (NotPromoted)");
}

const AUTOREF_PROGRAM: &str = r#"#![feature(no_core, lang_items, rustc_attrs, auto_traits)]
#![no_core]
#![no_main]

#[lang = "pointee_sized"]
trait PointeeSized {}

#[lang = "meta_sized"]
trait MetaSized: PointeeSized {}

#[lang = "sized"]
trait Sized: MetaSized {}

#[lang = "freeze"]
unsafe auto trait Freeze {}

#[lang = "legacy_receiver"]
trait LegacyReceiver: PointeeSized {}

impl<T: PointeeSized> LegacyReceiver for &T {}

#[lang = "copy"]
trait Copy {}

enum Fork {
    A,
    B,
    C,
}

impl Fork {
    fn as_ref(&self) -> &Self {
        self
    }
}

fn pick_autoref() -> &'static Fork {
    Fork::C.as_ref()
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let _ = pick_autoref();
    0
}
"#;

#[test]
fn a_promoted_autoref_evaluates_to_its_unadjusted_value() {
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    let files = [SourceFile { path: "src/main.rs", text: AUTOREF_PROGRAM }];
    let tc = Toolchain::none(alloc);
    let new = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind).unwrap_or_else(|e| panic!("texts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("inputs: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
    let mut front = Front2::new(&names, &frame, &files, alloc).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, alloc);
    let pick = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &["pick_autoref"] }, alloc)
        .ok()
        .and_then(|f| f.whole(1))
        .map(|d| d.key)
        .unwrap_or_else(|| panic!("`pick_autoref` from root"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    store.ensure(&names, &mut front.keys, Want::Fn(pick)).unwrap_or_else(|e| panic!("head: {e:?}"));
    let owner = front.keys.item(pick).unwrap_or_else(|_| panic!("owner key"));
    let body = lower_fn(&names, prepared, &mut front.keys, &mut store, pick).unwrap_or_else(|e| panic!("lower: {e}"));
    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
    let borrow = {
        let view = body.lowered.view();
        let spell = Spelling::of(&view).expect("spelling");
        let scope = body.scope(&spell);
        let checked = check_body(&mut records, &body, &scope).unwrap_or_else(|e| panic!("check: {e:?}"));
        assert!(checked.diags.is_empty(), "diags: {:?}", checked.diags);
        assert_eq!(checked.promoted.len(), 1, "expected 1 promoted autoref, got {:?}", checked.promoted);
        checked.promoted[0]
    };
    let mut consts = Consts::default();
    let key = promoted_key(owner, borrow, AUTOREF_PROGRAM);
    let rec = consts
        .body_value(&mut records, prepared, owner, BodyRoot::Promoted(borrow), key, blaze_typeck::PTys::EMPTY, &[], None, alloc)
        .unwrap_or_else(|e| panic!("body_value failed: {e}"));
    assert_eq!(rec.data.allocs.len(), 1, "single allocation for unadjusted enum value: {rec:?}");
    assert!(rec.data.allocs[0].relocs.is_empty(), "no relocations for unadjusted enum value: {rec:?}");
    assert_eq!(rec.data.allocs[0].bytes.as_slice(), &[2u8], "`Fork::C` discriminant is 2");
}
