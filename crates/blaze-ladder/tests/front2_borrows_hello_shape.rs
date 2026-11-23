//! The borrow check over bstd hello world's shape (blaze-std entry.rs `main!`): a macro whose
//! expansion nests the user's fn in the body of the generated `main` and calls it with a
//! borrowed argument, got through a head naming two lifetimes of one ADT
//! (`fn checking(&self) -> Checking<'_, 'g, ()>`, blaze-std grant/made.rs).
//!
//! - `main` (the expansion): CHECKED, `refused None`, no finding. Before one origin position per
//!   lifetime parameter (blaze-flow-lower `AdtFacts::lifetimes`), `checking`'s head was refused
//!   (`TwoLifetimesOneOrigin`) and `main` ended `CallWritesLifetimes`, its borrows not checked.
//! - `user` (the nested fn): `eat(t); eat(t);` is a use after move (the Reference, "Moved and
//!   copied types": E0382). It is NOT yet a listed body, so not yet checked: the next piece.
//!
//! Expected lines: `borrows main: [] refused None`, `user listed: false`. Needs the installed
//! toolchain under `$HOME/.blaze/v2/toolchain`.

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::borrows::BorrowVerdict;
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const EDITION: &str = "blaze-2026.11";

const MAIN: &str = "struct Given<'g> {
    root: &'g u8,
}

struct Checking<'c, 'g, M> {
    given: &'c Given<'g>,
    mark: M,
}

impl<'g> Given<'g> {
    fn checking(&self) -> Checking<'_, 'g, ()> {
        Checking { given: self, mark: () }
    }
}

impl<'c, 'g, M> Checking<'c, 'g, M> {
    fn root(&self) -> &'g u8 {
        self.given.root
    }
}

struct Tok;

fn eat(t: Tok) {
    let _ = t;
}

macro_rules! entry {
    ($user:item) => {
        fn main() {
            $user
            let a = 1u8;
            let given = Given { root: &a };
            let checking = given.checking();
            let _ = user(checking.root(), Tok);
        }
    };
}

entry! {
    fn user(cx: &u8, t: Tok) -> u8 {
        eat(t);
        eat(t);
        *cx
    }
}
";

fn shown(item: &str, v: &BorrowVerdict<'_>) -> String {
    let found: Vec<String> = v.findings.iter().map(|d| format!("{} at {:?}", d.code, item.get(d.start as usize..d.end as usize).unwrap_or("<out of range>"))).collect();
    format!("{found:?} refused {:?}", v.refused)
}

#[test]
fn a_macro_nested_user_fn_is_borrow_checked() {
    let heap = blaze_own::testing::heap();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hs/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hs", dir: "/hs/", root: "/hs/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
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
    let listed: Vec<String> = bodies.iter().map(|(b, _)| name_of(b)).collect();
    println!("bodies: {listed:?}");
    let find = |n: &str| bodies.iter().find(|(b, _)| name_of(b) == n).map(|(b, _)| *b).unwrap_or_else(|| panic!("`{n}` listed: {listed:?}"));
    let text_of = |b: BodyRef<'_>| {
        let k = match b {
            BodyRef::Free(k) => k,
            BodyRef::Member { container, .. } => container,
        };
        names.item_text(k).expect("the body's item text").text
    };

    // The expansion: `given.checking()` read by its head, `user(..)` by its own: checked, clean.
    let b = find("main");
    let (types, v) = world.check_borrows(b, heap.alloc());
    println!("borrows main: {}", shown(text_of(b), &v));
    assert!(types.is_clean(), "main's types: {types:?}");
    assert!(v.refused.is_none(), "main not checked: {}", shown(text_of(b), &v));
    assert!(v.findings.is_empty(), "main: {}", shown(text_of(b), &v));

    // The user's fn nested in it (`user`) is not yet a listed body (`ProjectWorld::bodies` lists
    // free fns and members, not items nested in a body): its check, and the E0382 it must
    // find, are the next piece; the line below says whether it is listed.
    println!("user listed: {}", listed.iter().any(|n| n == "user"));
}
