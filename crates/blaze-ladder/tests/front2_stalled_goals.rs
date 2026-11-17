//! `blaze check`'s path for an auto trait of an async block's future (front2 stalled_goals.rs,
//! wired into `ProjectWorld::check_borrows`): the type check files the goal that fails on its
//! superset interior as a stalled row (`blaze_typeck::check_deferring`), the borrow check lowers
//! the block's body once (borrows_nested.rs), its exact interior is computed from that lowering
//! (blaze-flow-lower `interior`), and the row is decided on it (`decide_stalled_listed`).
//!
//! The rule: the Reference, "Async blocks": the block's future is "roughly equivalent to an
//! enum with one variant per await point, where each variant stores the data needed to resume
//! from its corresponding point"; "Auto traits": `Send` holds when it holds for all it stores;
//! the library's alloc/src/rc.rs: `impl<T: ?Sized, A: Allocator> !Send for Rc<T, A> {}`.
//!
//! - `held_across`: `r: Rc<u8>` is used after the `.await`, so the future stores it and is not
//!   `Send`; `spawn<F: Future + Send>` asks it: ONE type diagnostic, E0277, its range inside
//!   the `spawn(..)` call, filed as the check files its own; the body is not refused.
//! - `dropped_before`: `r` is moved into `drop` before the `.await`, so the future stores no
//!   `Rc` and is `Send`: a clean type verdict.
//! - The control (rule 23), in the same test: `ProjectWorld::check` (types only) takes the plain
//!   check (`Stalls::Gap`), whose only answer for the goal is the typed gap: `dropped_before`
//!   is REFUSED there with `CheckError::Unported(INTERIOR_GAP)`. So its clean verdict above
//!   comes from the decision on the exact interior, not from the check. (The same failure is
//!   what `check_borrows` gives with check.rs's `check_deferring` arm taken out: the test then
//!   fails at "dropped_before: types".)
//!
//! Prints "stalled held_across: E0277 at \"spawn(..)\"" (the covered text, whole),
//! "stalled dropped_before: clean", "stalled control dropped_before (plain check): the interior gap".
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`.

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::check::CheckFail;
use blaze_ladder::front2::project::{BodyRef, ProjectWorld, Refused};
use blaze_ladder::refusal::SourceFile;

/// A plain Rust crate: its `std` is the library's (`std::rc::Rc`), as front2_opaque_auto's.
const EDITION: &str = "2024";

const MAIN: &str = "use std::future::Future;
use std::rc::Rc;

fn main() {}

fn spawn<F: Future + Send>(f: F) {}

fn held_across() {
    spawn(async {
        let r = Rc::new(1u8);
        async {}.await;
        drop(r);
    });
}

fn dropped_before() {
    spawn(async {
        let r = Rc::new(1u8);
        drop(r);
        async {}.await;
    });
}
";

#[test]
fn an_rc_across_an_await_is_not_send() {
    let heap = blaze_own::testing::heap();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello-world/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates).unwrap_or_else(|e| panic!("resolve_crates: {e}"));

    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command's frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, heap.alloc()).unwrap_or_else(|e| panic!("front2: {e}"));
    let bodies = world.bodies(heap.alloc());
    let name_of = |b: &BodyRef<'_>| -> String {
        match b {
            BodyRef::Free(k) => names.item_text(*k).and_then(|t| t.name).unwrap_or("?").to_string(),
            BodyRef::Member { name, .. } => name.to_string(),
        }
    };
    let find = |n: &str| bodies.iter().find(|(b, _)| name_of(b) == n).map(|(b, _)| *b).unwrap_or_else(|| panic!("`{n}` listed"));
    let text_of = |b: BodyRef<'_>| {
        let k = match b {
            BodyRef::Free(k) => k,
            BodyRef::Member { container, .. } => container,
        };
        names.item_text(k).expect("the body's item text").text
    };

    // Held across the `.await`: one E0277 among the body's type diagnostics, inside the call.
    let held = find("held_across");
    let (types, borrows) = world.check_borrows(held, heap.alloc());
    let item = text_of(held);
    assert!(types.refused.is_none() && types.gaps.is_empty(), "held_across: decided, not refused: {types:?} (borrows {borrows:?})");
    assert_eq!(types.diagnostics.len(), 1, "held_across: one diagnostic: {types:?}");
    let d = &types.diagnostics[0];
    assert_eq!(d.code, "E0277", "held_across: {types:?}");
    let call = item.find("spawn(").expect("the call") as u32;
    let call_end = item.rfind(");").expect("the call's end") as u32 + 1;
    let covered = item.get(d.start as usize..d.end as usize).unwrap_or("<out of range>");
    assert!(d.start >= call && d.end <= call_end && d.end > d.start, "the E0277 at {}..{} ({covered:?}) is not inside the `spawn(..)` call {call}..{call_end} of {item:?}", d.start, d.end);
    assert!(d.text.contains("Send"), "the E0277 names the unmet `Send` goal: {:?}", d.text);
    println!("stalled held_across: E0277 at {covered:?}");

    // Dropped before the `.await`: the exact interior holds no `Rc`: clean.
    let dropped = find("dropped_before");
    let (types, borrows) = world.check_borrows(dropped, heap.alloc());
    assert!(types.is_clean(), "dropped_before: types {types:?} (borrows {borrows:?})");
    assert!(types.reads.is_some(), "dropped_before: a clean verdict has its read list's digest: {types:?}");
    println!("stalled dropped_before: clean");

    // The control: the plain check (types only) cannot decide the goal: the typed gap.
    let plain = world.check(dropped, heap.alloc());
    let gap = matches!(
        &plain.refused,
        Some(Refused::Check(CheckFail::Check { error: blaze_typeck::CheckError::Unported(what), .. })) if *what == blaze_typeck::INTERIOR_GAP
    );
    assert!(gap, "dropped_before under the plain check is the interior gap: {plain:?}");
    println!("stalled control dropped_before (plain check): the interior gap");
}
