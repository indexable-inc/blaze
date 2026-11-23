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
//! - `ProjectWorld::check` (types only) takes the same production path (`Stalls::Decide`, the
//!   witness lowered, the row decided: borrows.rs `witness_and_decide`): `dropped_before` is
//!   clean there too and `held_across` the same E0277. Before, it took `Stalls::Gap` and
//!   refused `dropped_before` with the typed interior gap: a harness on another path than
//!   the command's. The control for the decision itself (rule 23) stays: with check.rs's
//!   `check_deferring` arm taken out the test fails at "dropped_before: types".
//!
//! Prints "stalled held_across: E0277 at \"spawn(..)\"" (the covered text, whole),
//! "stalled dropped_before: clean", "stalled stored Packet: clean; stored Rc<Packet>: E0277 at packet",
//! "stalled dropped_before (plain check): clean; held_across (plain check): E0277".
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

struct Packet(u8);

fn replay_packet() {
    spawn(async {
        drop(Rc::new(0u8));
        let packet = Packet(1);
        async {}.await;
        drop(packet);
    });
}

fn replay_rc_packet() {
    spawn(async {
        drop(Rc::new(0u8));
        let packet = Rc::new(Packet(1));
        async {}.await;
        drop(packet);
    });
}

fn held_across() {
    spawn(async move {
        let gone = Rc::new(2u8);
        drop(gone);
        let r = Rc::new(1u8);
        async {}.await;
        drop(r);
    });
}

fn dropped_before() {
    spawn(async move {
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
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));

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
    // The why-chain's held-slot step (src/front2/stalled_goals.rs `held_slots`): "`r`
    // (`Rc<u8, ..>`) is held across `async {}.await`".
    assert!(d.text.contains("`r` (`Rc<u8") && d.text.contains("is held across `async {}.await`"), "the saved binding and suspension: {:?}", d.text);
    assert!(!d.text.contains("`gone` ("), "a moved binding of the same type is absent from the saved state: {:?}", d.text);
    println!("stalled held_across: E0277 at {covered:?}");

    // Dropped before the `.await`: the exact interior holds no `Rc`: clean.
    let dropped = find("dropped_before");
    let (types, borrows) = world.check_borrows(dropped, heap.alloc());
    assert!(types.is_clean(), "dropped_before: types {types:?} (borrows {borrows:?})");
    assert!(types.reads.is_some(), "dropped_before: a clean verdict has its read list's digest: {types:?}");
    println!("stalled dropped_before: clean");

    // The exact state asks Send of a user ADT. Plant a non-Send stored
    // value at exactly the same suspension, keeping the poll-time Rc in both.
    let (packet, _) = world.check_borrows(find("replay_packet"), heap.alloc());
    assert!(packet.is_clean() && packet.reads.is_some(), "stored Packet(u8): {packet:?}");
    let (rc_packet, _) = world.check_borrows(find("replay_rc_packet"), heap.alloc());
    assert!(rc_packet.refused.is_none() && rc_packet.gaps.is_empty(), "{rc_packet:?}");
    assert_eq!(rc_packet.diagnostics.len(), 1, "{rc_packet:?}");
    let finding = &rc_packet.diagnostics[0];
    assert_eq!(finding.code, "E0277");
    assert!(finding.text.contains("`packet` (`") && finding.text.contains("is held across `async {}.await`"), "{finding:?}");
    println!("stalled stored Packet: clean; stored Rc<Packet>: E0277 at packet");

    // The plain check (types only) takes the production path too (project.rs `check`:
    // `Stalls::Decide`, the witness lowered, the row decided): the same verdict as the borrow
    // check's, never the old path's interior gap (`INTERIOR_GAP`).
    let plain = world.check(dropped, heap.alloc());
    let gap = matches!(
        &plain.refused,
        Some(Refused::Check(CheckFail::Check { error: blaze_typeck::CheckError::FutureWitness { reason, .. }, .. }))
            if *reason == blaze_typeck::INTERIOR_GAP
    );
    assert!(!gap, "dropped_before under the plain check must be decided, not the old path's interior gap: {plain:?}");
    assert!(plain.is_clean(), "dropped_before under the plain check: clean, as under check_borrows: {plain:?}");
    let held = world.check(find("held_across"), heap.alloc());
    assert!(held.diagnostics.iter().any(|d| d.code == "E0277"), "CONTROL: held_across under the plain check is the same E0277: {held:?}");
    println!("stalled dropped_before (plain check): clean; held_across (plain check): E0277");
}

/// Both outer futures await another fn's opaque future. Its initial state is what
/// differs: a u8 is Send, and an Rc<u8> is not, even if the first poll drops it.
#[test]
fn send_of_an_async_fn_follows_the_awaited_opaque() {
    const SOURCE: &str = r#"
use std::rc::Rc;
fn need_send<T: Send>(_: T) {}
async fn leaf<T>(value: T) { drop(value); }
async fn outer_send() { leaf(1u8).await; }
async fn outer_rc() { leaf(Rc::new(1u8)).await; }
fn pass() { need_send(outer_send()); }
fn control() { need_send(outer_rc()); }
fn main() {}
"#;
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("open the installed toolchain");
    let files = [("/nested/src/main.rs".to_owned(), SOURCE.to_owned())];
    let user = UserCrate {
        name: "nested", dir: "/nested/", root: "/nested/src/main.rs", files: &files,
        edition: edition::PackageEdition::of_manifest(EDITION).expect("Rust 2024"), panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).expect("toolchain and test crate");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("resolve names");
    let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("test frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("front end");
    let bodies = world.bodies(alloc);
    for name in ["pass", "control"] {
        let (body, _) = bodies.iter().find(|(b, _)| {
            matches!(b, BodyRef::Free(k) if names.item_text(*k).and_then(|t| t.name) == Some(name))
        }).expect("fixture function");
        let (types, _) = world.check_borrows(*body, alloc);
        assert!(types.refused.is_none() && types.gaps.is_empty() && types.undecided.is_empty(), "{name}: {types:?}");
        if name == "pass" {
            assert!(types.is_clean() && types.findings.is_empty() && types.reads.is_some(), "{name}: {types:?}");
            println!("opaque stored future: pass: clean");
        } else {
            let [d] = types.diagnostics.as_slice() else { panic!("one error for the Rc stored in the awaited future: {types:?}") };
            assert_eq!(d.code, "E0277", "{types:?}");
            assert!(d.text.contains("Send") && d.start < d.end, "{types:?}");
            println!("opaque stored future: control: E0277 Send");
        }
    }
}
