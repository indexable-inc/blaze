//! `blaze check`'s BORROWS stage entry (front2/borrows.rs, `ProjectWorld::check_borrows`) on a
//! copy of the owner's hello world with four more bodies, each checked by the real front end,
//! lowered to its flow (blaze-flow-lower) and move and borrow checked (blaze-flow).
//!
//! - `moved`: the Reference, "Moved and copied types": "When a place expression is evaluated
//!   in a value expression context ... If the type of that value implements Copy, then the
//!   value will be copied. In the remaining situations ... the value is moved"; and
//!   "Variables": a moved-from variable cannot be read until reinitialized. `Token` is not
//!   `Copy`, so `let c = a;` after `let b = a;` uses a moved value: ONE finding, E0382, whose
//!   range covers the second `a`.
//! - `both`: the Reference, "References and borrowing" (Pointer types, "Mutable references"):
//!   "&mut ... references ... a unique reference": while the shared `r = &c` is live (it is
//!   read after), `&mut c` is refused: ONE finding, E0502, whose range covers `&mut c`.
//! - `clean`: a move then a use of the new owner, nothing borrowed across a write: an empty
//!   verdict, and not refused.
//! - `with_closure`: a body with a closure: the closure's body is checked as its own flow
//!   (blaze-flow-lower `lower_closure`, front2 borrows_nested.rs) and the call of it is a
//!   shared use of the closure value: an empty verdict, and not refused.
//!
//! Prints, per body, a line `borrows <name>: <findings> refused <refusal>`; the expected ones:
//! "borrows moved: [E0382 at \"a\"] refused None", "borrows both: [E0502 at \"&mut c\"] refused
//! None", "borrows clean: [] refused None", "borrows with_closure: [] refused None". Needs the
//! installed toolchain under `$HOME/.blaze/v2/toolchain`.

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::borrows::BorrowVerdict;
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const EDITION: &str = "blaze-2026.11";

/// His `src/main.rs` as written, plus the four bodies.
const MAIN: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    io.need_out()?.line(cx, \"Hello, world!\")?;
    Ok(())
}

struct Token {
    n: u8,
}

fn moved() {
    let a = Token { n: 1 };
    let b = a;
    let c = a;
}

fn both() {
    let mut c = Token { n: 1 };
    let r = &c;
    let m = &mut c;
    let k = r;
}

fn clean() {
    let a = Token { n: 1 };
    let b = a;
    let n = b.n;
}

fn with_closure() {
    let f = || 1u8;
    let x = f();
}
";

/// One body's verdict as the line the test prints: each finding's code and the text its range
/// covers in the body's item text.
fn shown(item: &str, v: &BorrowVerdict<'_>) -> String {
    let found: Vec<String> = v.findings.iter().map(|d| format!("{} at {:?}", d.code, item.get(d.start as usize..d.end as usize).unwrap_or("<out of range>"))).collect();
    format!("{found:?} refused {:?}", v.refused)
}

#[test]
fn own_bodies_move_and_borrow_checked() {
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

    // A use after a move: one E0382 at the second `a`.
    let moved = find("moved");
    let (types, v) = world.check_borrows(moved, heap.alloc());
    let item = text_of(moved);
    println!("borrows moved: {}", shown(item, &v));
    assert!(types.is_clean(), "moved's types: {types:?}");
    assert!(v.refused.is_none(), "moved: {v:?}");
    assert_eq!(v.findings.len(), 1, "moved: {}", shown(item, &v));
    let d = v.findings[0];
    assert_eq!(d.code, "E0382", "moved: {}", shown(item, &v));
    let second = item.find("let c = a").expect("the second use") as u32 + "let c = ".len() as u32;
    assert!(d.start <= second && d.end >= second + 1, "the range {}..{} of {item:?} is not the second `a` at {second}", d.start, d.end);

    // A mutable borrow while a shared one is live: one E0502 at `&mut c`.
    let both = find("both");
    let (types, v) = world.check_borrows(both, heap.alloc());
    let item = text_of(both);
    println!("borrows both: {}", shown(item, &v));
    assert!(types.is_clean(), "both's types: {types:?}");
    assert!(v.refused.is_none(), "both: {v:?}");
    assert_eq!(v.findings.len(), 1, "both: {}", shown(item, &v));
    let d = v.findings[0];
    assert_eq!(d.code, "E0502", "both: {}", shown(item, &v));
    let covered = item.get(d.start as usize..d.end as usize).unwrap_or("");
    assert!(covered.contains("&mut c"), "the range {}..{} of {item:?} is {covered:?}, not `&mut c`", d.start, d.end);

    // A clean body: checked to its end, no finding.
    let clean = find("clean");
    let (types, v) = world.check_borrows(clean, heap.alloc());
    println!("borrows clean: {}", shown(text_of(clean), &v));
    assert!(types.is_clean(), "clean's types: {types:?}");
    assert!(v.is_clean(), "clean: {v:?}");

    // A body with a closure: the closure's body checked on its own; clean.
    let closure = find("with_closure");
    let (types, v) = world.check_borrows(closure, heap.alloc());
    println!("borrows with_closure: {}", shown(text_of(closure), &v));
    assert!(types.is_clean(), "with_closure's types: {types:?}");
    assert!(v.is_clean(), "with_closure: {v:?}");
}
