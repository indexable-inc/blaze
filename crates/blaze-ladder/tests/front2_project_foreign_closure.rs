//! A closure owned by a body checked on the RECORDS path (`blaze check`'s per-body entry,
//! front2/project.rs `ProjectWorld::check`, which checks through body_reads.rs
//! `check_body_filed`) is found when ANOTHER body asks for it (hello-d-2).
//!
//! The program: `make() -> impl Fn(u8) -> u8 { |x: u8| x }` and `uses() { is_send(make()); }`.
//! `uses` asks `Send` of `make`'s opaque, so its hidden type, the closure `Closure { owner:
//! make, ordinal: 0 }`, is read from `make`'s filed check (front2 opaque_auto.rs), and lowering
//! that type asks the world `closure(make, 0)`: its kind and signature, which only `make`'s
//! check found (typeck.md, "A closure's identity"; front2 foreign_closures.rs).
//!
//! The rules the program rests on (the Rust Reference):
//! - "Impl trait", "Auto trait leakage": "Auto traits ... of the hidden type are leaked through
//!   the opaque type", so `make()`'s opaque is `Send` when its hidden type is.
//! - "Closure types": a closure "has a unique, anonymous type"; "Other traits": "Send" is
//!   implemented "if the type of every capture is Send"; this closure captures nothing, so it
//!   is `Send`. And it "implements Fn" (it neither moves out of nor mutates a capture), so
//!   `make`'s body satisfies `impl Fn(u8) -> u8`.
//! - library/core/src/marker.rs: `pub unsafe auto trait Send { }`.
//!
//! THE CAUSE AND ITS CONTROL. Order A checks `make` FIRST on the records path, then `uses`:
//! before the fix `check_body_filed` filed `make`'s hidden types (opaque_auto) but not its
//! closures, and `make` is never checked again (rule 77: its hidden type is filed), so `uses`
//! stops on the gap "closure #0 of `make` .. its owner has no clean check in this command".
//! Order B (the control) checks `uses` FIRST: the opaque's work list checks `make` through
//! `check_body` (the asked path, which files closures since hello-d-1), so `uses` is clean with
//! or without this fix. Only the records path's filing differs between the two.
//!
//! Expected lines:
//!   `ORDER A (make first, records path): make: clean; uses: clean`
//!   `ORDER B (uses first): uses: clean; make: clean`
//!   `test a_closure_of_a_body_checked_on_the_records_path_is_found_by_another ... ok`
//! Without the fix ORDER A's `uses` line holds "closure #0 of" and "no clean check".
//!
//! Needs wiring/blaze-typeck/opaque-auto-1.md and wiring/blaze-ladder/opaque-auto-2.md applied
//! (the hidden-type leak), and the installed toolchain under `$HOME/.blaze/v2/toolchain`.
//! NEVER COMPILED by its writer (a write-only lane).

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const RUST_EDITION: &str = "2024";

const PROGRAM: &str = "fn is_send<T: Send>(_: T) {}
fn make() -> impl Fn(u8) -> u8 { |x: u8| x }
fn uses() { is_send(make()); }
fn main() {}
";

/// The bodies named in `order`, checked in that order over ONE `ProjectWorld` (the records
/// path), each with its verdict shown: `clean`, or its diagnostics' codes, gaps and refusal.
fn check_in_order(order: &[&str]) -> Vec<(String, String)> {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello-world/src/main.rs".to_string(), PROGRAM.to_string())];
    let edition = edition::PackageEdition::of_manifest(RUST_EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: PROGRAM }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command's frame");
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).unwrap_or_else(|e| panic!("front2: {e}"));
    let bodies = world.bodies(alloc);
    let name_of = |b: &BodyRef<'_>| -> String {
        match b {
            BodyRef::Free(k) => names.item_text(*k).and_then(|t| t.name).unwrap_or("?").to_string(),
            BodyRef::Member { name, .. } => name.to_string(),
        }
    };
    let mut got = Vec::new();
    for &n in order {
        let body = bodies.iter().find(|(b, _)| name_of(b) == n).map(|(b, _)| *b).unwrap_or_else(|| panic!("`{n}` listed"));
        let v = world.check(body, alloc);
        let shown = if v.is_clean() {
            "clean".to_string()
        } else {
            let diags: Vec<&str> = v.diagnostics.iter().map(|d| d.code).collect();
            let gaps: Vec<String> = v.gaps.iter().map(|g| format!("{g}")).collect();
            format!("diags {diags:?}; gaps {gaps:?}; refused {:?}", v.refused)
        };
        got.push((n.to_string(), shown));
    }
    got
}

fn line(got: &[(String, String)]) -> String {
    got.iter().map(|(n, o)| format!("{n}: {o}")).collect::<Vec<_>>().join("; ")
}

/// Order A fails without the fix (the records path filed no closure); order B is the control.
#[test]
fn a_closure_of_a_body_checked_on_the_records_path_is_found_by_another() {
    let a = check_in_order(&["make", "uses"]);
    println!("ORDER A (make first, records path): {}", line(&a));
    let b = check_in_order(&["uses", "make"]);
    println!("ORDER B (uses first): {}", line(&b));
    assert!(b.iter().all(|(_, o)| o == "clean"), "the control: `uses` first defines `make` through the asked path: {}", line(&b));
    assert_eq!(a[0].1, "clean", "`make` checks clean: {}", line(&a));
    assert!(
        !a[1].1.contains("closure #0 of") && !a[1].1.contains("no clean check"),
        "`make`'s closure, filed where its records-path check finished, is found: {}",
        line(&a)
    );
    assert_eq!(a[1].1, "clean", "`uses` reads `make`'s closure from its filed check: {}", line(&a));
}
