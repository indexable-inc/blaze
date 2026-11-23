//! An `if`'s or `match`'s arm is a temporary scope of its own, and a `let` extends through it.
//!
//! The rules (the Reference, "Destructors", "Temporary scopes"): the smallest scope containing a
//! temporary is among "the body of an `if`", "the `else` block of an `if`", "the body
//! expression for a match arm" (rustc region.rs: both `if` branches and each arm are
//! terminating scopes), in every edition. So `look(match c { _ => make(&mk()) })` and
//! `look(if c { make(&mk()) } else { .. })` are E0716: `mk()` dies at the arm's end, before
//! `look` reads the reference made from it. "Temporary lifetime extension": in a `let`
//! initializer the operand of an extending borrow lives to the end of the block holding the
//! `let`; extending expressions include "the final expression of any extending block
//! expression", "the arm(s) of an extending match expression" and "the final expressions of an
//! extending if expression's consequent, else if, and else blocks" (Rust 1.79). A call's
//! argument is not extending.
//!
//! blaze-flow-lower lib.rs `close_arm_temps` (the arm's scope), `mark_extended` (through the
//! arms), `extend_temp` (to the `let`'s block, not the innermost open one). Before, an arm's
//! temporaries lived to the statement's end: `match_arm` and `if_arm` were accepted, a
//! dangling borrow unreported.
//! NOT YET RUN: written by a write-only lane (2026-11-21). Every verdict printed first.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const SOURCE: &str = r#"
pub struct S;
pub fn mk() -> S { S }
pub fn make(s: &S) -> &S { s }
pub fn look(_s: &S) {}

fn match_arm(c: bool) {
    look(match c { _ => make(&mk()) });
}
fn match_arm_let(c: bool) {
    let x = match c { _ => &mk() };
    look(x);
}
fn match_arm_let_arg(c: bool) {
    let x = match c { _ => make(&mk()) };
    look(x);
}
fn if_arm(c: bool) {
    look(if c { make(&mk()) } else { make(&mk()) });
}
fn if_arm_let(c: bool) {
    let x = if c { &mk() } else { &mk() };
    look(x);
}
fn if_arm_let_arg(c: bool) {
    let x = if c { make(&mk()) } else { make(&mk()) };
    look(x);
}
fn if_let_arm(o: Option<u8>) {
    look(if let Some(_) = o { make(&mk()) } else { make(&mk()) });
}
fn block_let() {
    let x = { &mk() };
    look(x);
}

fn main() {}
"#;

const NAMES: [&str; 8] = ["match_arm", "match_arm_let", "match_arm_let_arg", "if_arm", "if_arm_let", "if_arm_let_arg", "if_let_arm", "block_let"];

/// Each body's verdict in a crate of `edition`: `Some(true)` a borrow finding, `Some(false)`
/// clean, `None` refused (not checked), each printed.
fn verdicts(edition: &str) -> Vec<(&'static str, Option<bool>)> {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/arms/src/main.rs";
    let files = [(root.to_owned(), SOURCE.to_owned())];
    let user = UserCrate {
        name: "arms",
        dir: "/arms/",
        root,
        files: &files,
        edition: edition::PackageEdition::of_manifest(edition).expect("an edition"),
        panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).expect("fixture crate");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("body checker");
    let key_of = |name: &str| names.items(0).find_map(|(key, _, _)| (names.item_text(key)?.name == Some(name)).then_some(key)).expect(name);
    let mut out = Vec::new();
    for name in NAMES {
        let (types, v) = world.check_borrows(BodyRef::Free(key_of(name)), alloc);
        println!("{edition} {name}: types clean {}, {v:?}", types.is_clean());
        assert!(types.is_clean(), "{edition} `{name}`'s types: {types:?}");
        let got = if v.refused.is_some() { None } else { Some(!v.findings.is_empty()) };
        out.push((name, got));
    }
    out
}

/// In 2021 and in 2024 alike: an arm's temporary borrowed past the arm is a finding
/// (`match_arm`, `if_arm`, `if_let_arm`: E0716); its `let`-extended TWIN is clean
/// (`match_arm_let`, `if_arm_let`, `block_let`); the `let` whose arm passes the borrow to a call
/// is not extended and is a finding (`match_arm_let_arg`, `if_arm_let_arg`).
#[test]
fn an_arms_temporaries_die_at_its_end_unless_a_let_extends_them() {
    let want = [
        ("match_arm", Some(true)),
        ("match_arm_let", Some(false)),
        ("match_arm_let_arg", Some(true)),
        ("if_arm", Some(true)),
        ("if_arm_let", Some(false)),
        ("if_arm_let_arg", Some(true)),
        ("if_let_arm", Some(true)),
        ("block_let", Some(false)),
    ];
    let old = verdicts("2021");
    let new = verdicts("2024");
    println!("2021: {old:?}\n2024: {new:?}");
    assert_eq!(old, want, "2021");
    assert_eq!(new, want, "2024");
}
