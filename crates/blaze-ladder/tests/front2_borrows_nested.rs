//! The move and borrow check over closures and async bodies (front2 borrows_nested.rs, wired
//! into `ProjectWorld::check_borrows`; blaze-flow-lower closure.rs `lower_closure`,
//! `lower_async_fn`, coroutine.rs `lower_async`): each closure's and async block's body is
//! checked as a flow of its own, and an `async fn`'s body with its suspensions, instead of the
//! whole body being refused (`Form::Closure`, `Form::AsyncBlock`, `Form::Coroutine`).
//!
//! - `closure_conflict`: the Reference, "Capture modes": `n += 1` in the closure captures `n` by
//!   mutable borrow, made where the closure is and held by the closure value while it is live
//!   ("Closure types": the closure is a struct of its captures); "References and borrowing"
//!   ("Mutable references": unique): `&n` while `bump` is still called after it is ONE finding,
//!   E0502, whose range covers `&n`.
//! - `clean_closure`: a closure capturing `v` by shared borrow, called, then `v.n` read: shared
//!   with shared, no finding, not refused.
//! - `moved_into_closure`: the Reference, "Capture modes": with `move` "all captures are by move
//!   or, for `Copy` types, by copy"; `Token` is not `Copy`, so the closure moves `t` where it is
//!   made, and "Moved and copied types": `let u = t;` after it uses a moved value: ONE finding,
//!   E0382, whose range covers the `t` of `let u = t`. The closure's own body (`let k = t;`,
//!   `FnOnce`: it owns `t`) is clean.
//! - `clean_async`: "Async blocks": the block captures `t` by reference; "Await expressions":
//!   the block suspends at `.await` and resumes there, so `r = &t` stays valid across it and
//!   `r.n` after it is fine: no finding, not refused.
//! - `clean_async_fn`: "Async functions": the same over an `async fn`'s own body (its parameter
//!   moved into the future, borrowed across the `.await`): no finding, not refused.
//!
//! Without the wiring every one of these bodies is refused (`Lower(.. Closure ..)`,
//! `Lower(.. AsyncBlock ..)`, `Lower(.. Coroutine ..)`) with no finding, and each assertion on
//! `refused.is_none()` fails.
//!
//! Prints, per body, `borrows <name>: <findings> refused <refusal>`; the expected lines:
//! "borrows closure_conflict: [\"E0502 at \\\"&n\\\"\"] refused None",
//! "borrows clean_closure: [] refused None",
//! "borrows moved_into_closure: [\"E0382 at \\\"t\\\"\"] refused None",
//! "borrows clean_async: [] refused None", "borrows clean_async_fn: [] refused None".
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`.

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::borrows::BorrowVerdict;
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const EDITION: &str = "blaze-2026.11";

const MAIN: &str = "fn main() {}

struct Token {
    n: u8,
}

fn closure_conflict() {
    let mut n = 1u8;
    let mut bump = || n += 1;
    let r = &n;
    bump();
}

fn clean_closure() {
    let v = Token { n: 1 };
    let get = || v.n;
    let a = get();
    let b = v.n;
}

fn moved_into_closure() {
    let t = Token { n: 1 };
    let f = move || {
        let k = t;
    };
    let u = t;
}

fn clean_async() {
    let t = Token { n: 1 };
    let fut = async {
        let r = &t;
        let k = async { 1u8 }.await;
        let m = r.n;
    };
}

async fn clean_async_fn(t: Token) -> u8 {
    let r = &t;
    let k = async { 1u8 }.await;
    r.n
}
";

/// One body's verdict as the line the test prints: each finding's code and the text its range
/// covers in the body's item text.
fn shown(item: &str, v: &BorrowVerdict<'_>) -> String {
    let found: Vec<String> = v.findings.iter().map(|d| format!("{} at {:?}", d.code, item.get(d.start as usize..d.end as usize).unwrap_or("<out of range>"))).collect();
    format!("{found:?} refused {:?}", v.refused)
}

#[test]
fn closures_and_async_bodies_are_move_and_borrow_checked() {
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

    // The owner reads what its closure holds mutably borrowed: one E0502 at `&n`.
    let conflict = find("closure_conflict");
    let (types, v) = world.check_borrows(conflict, heap.alloc());
    let item = text_of(conflict);
    println!("borrows closure_conflict: {}", shown(item, &v));
    assert!(types.is_clean(), "closure_conflict's types: {types:?}");
    assert!(v.refused.is_none(), "closure_conflict: {v:?}");
    assert_eq!(v.findings.len(), 1, "closure_conflict: {}", shown(item, &v));
    let d = v.findings[0];
    assert_eq!(d.code, "E0502", "closure_conflict: {}", shown(item, &v));
    let covered = item.get(d.start as usize..d.end as usize).unwrap_or("");
    assert!(covered.contains("&n"), "the range {}..{} of {item:?} is {covered:?}, not `&n`", d.start, d.end);

    // Clean bodies, each checked to its end: no finding, not refused.
    for name in ["clean_closure", "clean_async", "clean_async_fn"] {
        let b = find(name);
        let (types, v) = world.check_borrows(b, heap.alloc());
        println!("borrows {name}: {}", shown(text_of(b), &v));
        assert!(types.is_clean(), "{name}'s types: {types:?}");
        assert!(v.is_clean(), "{name}: {}", shown(text_of(b), &v));
    }

    // A capture by move, then a use of the moved variable: one E0382 at the `t` of `let u = t`.
    let moved = find("moved_into_closure");
    let (types, v) = world.check_borrows(moved, heap.alloc());
    let item = text_of(moved);
    println!("borrows moved_into_closure: {}", shown(item, &v));
    assert!(types.is_clean(), "moved_into_closure's types: {types:?}");
    assert!(v.refused.is_none(), "moved_into_closure: {v:?}");
    assert_eq!(v.findings.len(), 1, "moved_into_closure: {}", shown(item, &v));
    let d = v.findings[0];
    assert_eq!(d.code, "E0382", "moved_into_closure: {}", shown(item, &v));
    let used = item.find("let u = t").expect("the later use") as u32 + "let u = ".len() as u32;
    assert!(d.start <= used && d.end >= used + 1, "the range {}..{} of {item:?} is not the `t` of `let u = t` at {used}", d.start, d.end);
}
