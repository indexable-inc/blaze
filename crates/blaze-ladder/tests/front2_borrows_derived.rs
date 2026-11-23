//! Piece BR4: the borrow check's false findings in derived `eq`/`partial_cmp` (census 3 row 1
//! and 4) and in `write!` (row 2), PRINTED through the path `blaze check` takes
//! (front2/borrows_print.rs `print_borrows`: the type check's record, `gather_facts`,
//! blaze-flow-lower `lower`, blaze-flow `check`), with controls that must stay found.
//!
//! Rules (the Reference): "Generic parameters" (every occurrence of `T` in a signature is the
//! one type argument of the call); "Lifetime elision" ("Each elided lifetime in the parameters
//! becomes a distinct lifetime parameter"); "Subtyping and Variance" (`&'a T` covariant in
//! `'a` and `T`, `&'a mut T` invariant in `T`); "Lifetime bounds" (`&'a T` well formed only
//! when `T: 'a`). The derive's text is blaze-expand derive.rs `fields_eq`:
//! `$crate::cmp::PartialEq::eq(&__self_0, &__arg1_0)`, with `__self_0: &&'a str` bound by
//! default binding mode under `match (self, other)`.
//!
//! Expected (each body prints its flow, then `findings N`, then the line below):
//! - "borrows eq: findings 0": derived `PartialEq::eq` of `Named<'a>`. WITHOUT the fix (call_done
//!   taking `unify_call` for a known head whose type argument `Self = &&'a str` carries origins)
//!   it is 2 or more: UnprovenOutlives at the `$crate::cmp::PartialEq::eq(..)` call and
//!   Invalidated over the arm (census 3: 68 + 66). In the printed flow WITHOUT the fix, at the
//!   `call(` statement the line holds `; oL: oH` where `oL` is the origin of the loan
//!   `[Lk in oL]` taken by `&__self_0` and `oH` the call's hub, and `; oH: oX` for an origin
//!   `oX` of `__self_0`'s own type: the loan flowing back out of the hub. WITH the fix no
//!   `subset` out of `oL` goes anywhere but a fresh origin of the call.
//! - "borrows partial_cmp: findings 0": derived `PartialOrd::partial_cmp`, the same class.
//! - "borrows write: findings 0": `write!(f, "{rights} {}", self.name)` with a local `rights`.
//!   WITHOUT the fix (the `format_args!` aggregate through `unify_call`) 2 or more: the loan of
//!   `rights` reaches `'a` of `self.name` through the hub.
//! - "borrows derived fmt: findings N": derived `Debug::fmt`: NOT diagnosed by reading; no
//!   assertion. If N > 0, look in the printed flow for the first `subset` that puts the origin
//!   of a loan taken in the arm (`&ra_expand_builder`, `&__self_0`) into an origin of a
//!   placeholder (`self`'s or `f`'s): its statement names the call that relates it (a
//!   `DebugStruct::field` through `call_by_signature`, or `Formatter::debug_struct`).
//! - CONTROLS, must stay found: "borrows control_put: findings N" with N >= 1: a local's loan
//!   stored through the generic `put<T>(slot: &mut T, value: T)` into `&mut &'a u8` (E0597 in
//!   rustc's words: `local` does not live long enough); "borrows control_moved: findings 1": a
//!   use after a move.
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`.

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::borrows_print::NotPrinted;
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const EDITION: &str = "blaze-2026.11";

const MAIN: &str = "fn main() {}

#[derive(PartialEq, PartialOrd, Debug)]
struct Named<'a> {
    name: &'a str,
    n: u8,
}

struct Shown<'a> {
    name: &'a str,
}

impl core::fmt::Display for Shown<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let rights = \"r\";
        write!(f, \"{rights} {}\", self.name)
    }
}

fn put<T>(slot: &mut T, value: T) {
    *slot = value;
}

fn control_put<'a>(slot: &mut &'a u8) {
    let local = 5u8;
    put(slot, &local);
}

struct Token {
    n: u8,
}

fn control_moved() {
    let a = Token { n: 1 };
    let b = a;
    let c = a;
}
";

/// The number on the printed flow's `findings N` line.
fn findings(printed: &str) -> usize {
    let line = printed.lines().rev().find(|l| l.starts_with("findings ")).unwrap_or_else(|| panic!("no `findings` line in:\n{printed}"));
    line["findings ".len()..].trim().parse().expect("a count")
}

#[test]
fn derived_and_fmt_bodies_printed() {
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
    let listed: Vec<(BodyRef<'_>, String, String)> = bodies
        .iter()
        .map(|(b, _)| {
            let (name, container) = match b {
                BodyRef::Free(k) => (names.item_text(*k).and_then(|t| t.name).unwrap_or("?").to_string(), *k),
                BodyRef::Member { name, container, .. } => (name.to_string(), *container),
            };
            let text = names.item_text(container).map(|t| t.text.to_string()).unwrap_or_default();
            (*b, name, text)
        })
        .collect();
    let one = |name: &str, text_has: &str| -> BodyRef<'_> {
        let found: Vec<BodyRef<'_>> = listed.iter().filter(|(_, n, t)| n == name && t.contains(text_has)).map(|(b, _, _)| *b).collect();
        assert_eq!(found.len(), 1, "one body `{name}` whose item text holds {text_has:?}; listed: {:?}", listed.iter().map(|(_, n, _)| n).collect::<Vec<_>>());
        found[0]
    };

    // `body` takes its type from the first call: written `BodyRef<'_>`, the closure would take
    // any lifetime and the world's `print_borrows` could not keep it.
    let mut print = |label: &str, body| -> usize {
        let mut out = String::new();
        let r: Result<(), NotPrinted> = world.print_borrows(body, heap.alloc(), &mut out);
        println!("== {label}\n{out}");
        if let Err(why) = r {
            panic!("{label}: not printed: {why:?}");
        }
        let n = findings(&out);
        println!("borrows {label}: findings {n}");
        n
    };

    let eq = print("eq", one("eq", ""));
    let partial_cmp = print("partial_cmp", one("partial_cmp", ""));
    let write = print("write", one("fmt", "write!"));
    let derived_fmt_body = listed.iter().find(|(_, n, t)| n == "fmt" && !t.contains("write!")).map(|(b, _, _)| *b).expect("the derived `fmt`");
    let derived_fmt = print("derived fmt", derived_fmt_body);
    let control_put = print("control_put", one("control_put", ""));
    let control_moved = print("control_moved", one("control_moved", ""));

    // The controls first: a fix that drops true findings fails here.
    assert!(control_put >= 1, "control_put: a local's loan stored into `&mut &'a u8` must be found");
    assert_eq!(control_moved, 1, "control_moved: one use after a move");
    assert_eq!(eq, 0, "derived eq");
    assert_eq!(partial_cmp, 0, "derived partial_cmp");
    assert_eq!(write, 0, "write! with a local argument");
    let _ = derived_fmt;
}
