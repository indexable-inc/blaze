//! A borrow returned through `Self::Value` of an impl of a trait WITH a lifetime (serde's
//! visitors; head record 29, ladder types.rs `Through::SelfImpl`), checked by the MOVE AND BORROW
//! check, under the old call paths (production) and under the one call rule (the census).
//!
//! Why this file: front2_futures_transitive.rs `a_borrow_through_self_name_of_an_impl_with_a_
//! lifetime_is_checked` asks E0515 of `ProjectWorld::check`, which is the TYPE check only
//! (project.rs `check`: `check_then` and the witness decision; it never calls `borrows_of`, and
//! its verdict's `typed` is always `None`, as the failing run printed). The borrow check is
//! `ProjectWorld::check_borrows` (borrows.rs `check_borrows_in`). So that test's empty
//! `diagnostics` says nothing of the borrow check; this one asks it.
//!
//! The rule (the Reference, "References"; rustc E0515): `V.visit(&n)` is `<V as Visitor<'de>>::
//! Value`, which the impl makes `&'de u8`, and `'de` is the region of the argument `x: &'de u8`.
//! A borrow of the local `n` returned through it as `&'static u8` outlives `n`.
//!
//! - `escapes` (by-value receiver): E0515, under both rules where the one rule is whole.
//! - TWIN `stays`: a `&'static u8` through the same call: clean.
//! - TWIN `passes` (the coordinator's): a `'de`-borrowed value returned as `&'a u8`: clean.
//! - `escapes_ref` (`&self` receiver): never clean. The output's region is the ARGUMENT's, not
//!   the receiver's: an elision guess from `&self` (lib.rs `call_old`'s elision arm) would miss it.
//! - TWIN `passes_ref`: the receiver is a local, the result borrows only the argument: no finding
//!   (rustc accepts it); the same elision guess would report a false E0515 here.
//!
//! NOT YET RUN (written 2026-11-21 by a write-only lane). Needs the installed toolchain under
//! `$HOME/.blaze/v2/toolchain`.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::borrows::BorrowRefused;
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const SOURCE: &str = r#"
pub trait Visitor<'de> { type Value; fn visit(self, x: &'de u8) -> Self::Value; }
pub trait VisitRef<'de> { type Value; fn visit_ref(&self, x: &'de u8) -> Self::Value; }
pub struct V;
impl<'de> Visitor<'de> for V { type Value = &'de u8; fn visit(self, x: &'de u8) -> Self::Value { x } }
impl<'de> VisitRef<'de> for V { type Value = &'de u8; fn visit_ref(&self, x: &'de u8) -> Self::Value { x } }

fn escapes() -> &'static u8 { let n = 1u8; V.visit(&n) }
fn stays(x: &'static u8) -> &'static u8 { V.visit(x) }
fn passes<'a>(x: &'a u8) -> &'a u8 { V.visit(x) }
fn escapes_ref() -> &'static u8 { let n = 1u8; V.visit_ref(&n) }
fn passes_ref<'a>(x: &'a u8) -> &'a u8 { let v = V; v.visit_ref(x) }

fn main() {}
"#;

const BODIES: [&str; 5] = ["escapes", "stays", "passes", "escapes_ref", "passes_ref"];

#[test]
fn a_borrow_returned_through_self_name_is_found_by_the_borrow_check() {
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/selfname/src/main.rs";
    let files = [(root.to_owned(), SOURCE.to_owned())];
    let user = UserCrate {
        name: "selfname",
        dir: "/selfname/",
        root,
        files: &files,
        edition: edition::PackageEdition::of_manifest("2024").expect("Rust 2024"),
        panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).expect("fixture inputs");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("body checker");
    let key_of = |name: &str| names.items(0).find_map(|(key, _, _)| (names.item_text(key)?.name == Some(name)).then_some(key)).expect("fixture body");

    // Under the one call rule (production): every verdict printed first.
    let mut got: Vec<(&str, Vec<&str>, Option<BorrowRefused>)> = Vec::new();
    for name in BODIES {
        let (types, borrows) = world.check_borrows(BodyRef::Free(key_of(name)), alloc);
        assert!(types.is_clean(), "{name}'s types: {types:?}");
        let codes: Vec<&str> = borrows.findings.iter().map(|d| d.code).collect();
        std::println!("one rule: {name}: codes {codes:?}, refused {:?}", borrows.refused.as_ref().map(|r| std::format!("{r}")));
        got.push((name, codes, borrows.refused));
    }
    let of = |n: &str| got.iter().find(|r| r.0 == n).expect("checked above");

    // The oracle: rustc's E0515, checked whole.
    for n in ["escapes", "escapes_ref"] {
        let (_, codes, refused) = of(n);
        assert_eq!(codes.as_slice(), ["E0515"], "`{n}`: the borrow of `n` returned through `Self::Value` (rustc E0515); refused {refused:?}");
        assert!(refused.is_none(), "`{n}` checked whole: {refused:?}");
    }
    // TWINS: the same call on a `&'static u8`, a `'de`-borrowed value returned as itself, and
    // `&self` receiver with local receiver + borrowed argument: all clean and checked whole.
    for n in ["stays", "passes", "passes_ref"] {
        let (_, codes, refused) = of(n);
        assert!(codes.is_empty() && refused.is_none(), "TWIN `{n}`: rustc accepts it; checked clean: {codes:?} {refused:?}");
    }
}
