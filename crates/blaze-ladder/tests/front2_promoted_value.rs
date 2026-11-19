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
    let names = inputs.resolve(&crates, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
    let mut front = Front2::new(&names, &frame, &files, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let pick = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &["pick"] }, heap.alloc())
        .ok()
        .and_then(|f| f.whole(1))
        .map(|d| d.key)
        .unwrap_or_else(|| panic!("`pick` from the crate root"));
    let prepared = Prepares::new_in(heap.alloc());
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

/// A borrow of a promoted borrow, and a table of them (ignore's `DEFAULT_TYPES` shape, arrays
/// so no unsizing): the checker promotes every borrow (blaze-typeck rules/promote.rs, a shared
/// borrow of a promotable `Freeze` operand), and each outer one evaluates to its operand with
/// the inner values as allocations of its own record, each pointer word relocated to one
/// (`Local(n)`, as a reference into a constant travels with it). Without the checker's rule
/// for a borrow inside a promoted operand only the inner borrows are promoted (`nest` 1 of 2,
/// `table` 2 of 3) and the outer ones are no constants at all.
const NESTED: &str = r#"#![feature(no_core, lang_items, rustc_attrs)]
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

fn nest() -> &'static &'static i32 {
    &&7
}

fn table() -> &'static [(&'static [u8; 1], &'static [u8; 2]); 1] {
    &[(&[1], &[2, 3])]
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    **nest()
}
"#;

/// Each promoted borrow of fn `name` of `program`, evaluated: one line per record, its
/// allocations as `bytes` with `@offset->target` per relocation, sorted.
fn promoted_records(program: &'static str, name: &str) -> Vec<String> {
    let heap = blaze_own::testing::heap();
    let files = [SourceFile { path: "src/main.rs", text: program }];
    let tc = Toolchain::none(blaze_own::Alloc::global());
    let new = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind).unwrap_or_else(|e| panic!("the program's texts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
    let mut front = Front2::new(&names, &frame, &files, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let item = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[name] }, heap.alloc())
        .ok()
        .and_then(|f| f.whole(1))
        .map(|d| d.key)
        .unwrap_or_else(|| panic!("`{name}` from the crate root"));
    let prepared = Prepares::new_in(heap.alloc());
    let mut buckets = None;
    store.ensure(&names, &mut front.keys, Want::Fn(item)).unwrap_or_else(|e| panic!("{name}'s head: {e:?}"));
    let owner = front.keys.item(item).unwrap_or_else(|_| panic!("{name}'s checker key"));
    let body = lower_fn(&names, prepared, &mut front.keys, &mut store, item).unwrap_or_else(|e| panic!("{name} lowers: {e}"));
    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
    let promoted: Vec<blaze_typeck::ExprId> = {
        let view = body.lowered.view();
        let spell = Spelling::of(&view).expect("the spelling");
        let scope = body.scope(&spell);
        let checked = check_body(&mut records, &body, &scope).unwrap_or_else(|e| panic!("{name} did not check: {e:?}"));
        assert!(checked.diags.is_empty(), "{name} checks with no diagnostic: {:?}", checked.diags);
        checked.promoted.iter().copied().collect()
    };
    let mut consts = Consts::default();
    let mut lines = Vec::new();
    for &borrow in &promoted {
        let key = promoted_key(owner, borrow, program);
        let line = match consts.body_value(&mut records, prepared, owner, BodyRoot::Promoted(borrow), key, blaze_typeck::PTys::EMPTY, &[], None, heap.alloc()) {
            Ok(rec) => {
                let allocs: Vec<String> = rec
                    .data
                    .allocs
                    .iter()
                    .map(|a| {
                        let bytes: Vec<String> = a.bytes.iter().map(|b| format!("{b:02x}")).collect();
                        let relocs: Vec<String> = a.relocs.iter().map(|r| format!(" @{}->{:?}", r.offset, r.target)).collect();
                        format!("[{}{}{}]", bytes.join(" "), relocs.join(""), if a.mutable { " mut" } else { "" })
                    })
                    .collect();
                allocs.join(" ")
            }
            Err(e) => format!("refused: {e}"),
        };
        println!("{name}: {line}");
        lines.push(line);
    }
    lines.sort();
    lines
}

#[test]
fn a_promoted_borrow_of_promoted_borrows_holds_their_values() {
    assert_eq!(
        promoted_records(NESTED, "nest"),
        ["[00 00 00 00 00 00 00 00 @0->Local(1)] [07 00 00 00]", "[07 00 00 00]"],
        "`&7`, and `&&7` as a word pointing at its own 7"
    );
    assert_eq!(
        promoted_records(NESTED, "table"),
        [
            "[00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 @0->Local(1) @8->Local(2)] [01] [02 03]",
            "[01]",
            "[02 03]",
        ],
        "`&[1]`, `&[2, 3]`, and the table as two words pointing at its own copies"
    );
}
