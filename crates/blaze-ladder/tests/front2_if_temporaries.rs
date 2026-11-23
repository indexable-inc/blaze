//! Where an `if` condition's temporaries die (blaze-flow-lower lib.rs, the `Expr::If` arm), by
//! the Reference, "Destructors", "Temporary scopes", and rustc region.rs `resolve_expr`
//! (`ScopeData::IfThenRescope` from 2024, `IfThen` before; the condition's `DropTemps`):
//!
//! 1. 2024 `if let` with an `else`: the scrutinee's temporaries ("the pattern-matching
//!    condition(s) and consequent body of if" are one temporary scope) are dropped before the
//!    `else` block. `if let Some(_v) = lock(&mut n).get() {} else { n += 1; }` is accepted in
//!    2024; in 2021 the guard lives to the end of the statement, so `n += 1` while it holds
//!    `&mut n` is E0506. TWIN (both editions): the same write in the THEN block is E0506 (the
//!    guard lives through the consequent body).
//! 2. A condition with no `let` is a temporary scope of its own in every edition: `if
//!    lock(&mut n).ok() { n += 1; }` is accepted. TWIN: the guard in a `let` (it outlives the
//!    `if`) is E0506.
//!
//! tempo is a 2024 workspace: every `if let Some(x) = m.lock().get(..) { } else { .. }` in it
//! was held too long in the else arm (false "borrowed" notes). Every verdict is printed first.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const SOURCE: &str = r#"
pub struct G<'a>(&'a mut u32);
impl Drop for G<'_> { fn drop(&mut self) {} }
impl G<'_> {
    fn get(&self) -> Option<u32> { Some(*self.0) }
    fn ok(&self) -> bool { *self.0 > 0 }
}
fn lock(x: &mut u32) -> G<'_> { G(x) }

fn if_let_else(mut n: u32) { if let Some(_v) = lock(&mut n).get() { } else { n += 1; } }
fn if_let_then(mut n: u32) { if let Some(_v) = lock(&mut n).get() { n += 1; } }
fn plain_cond(mut n: u32) { if lock(&mut n).ok() { n += 1; } }
fn plain_cond_twin(mut n: u32) { let g = lock(&mut n); if g.ok() { n += 1; } drop(g); }

fn main() {}
"#;

/// Each body's verdict in a crate of `edition`: `Some(true)` a borrow finding, `Some(false)`
/// clean, `None` refused (not checked), with the verdict printed.
fn verdicts(edition: &str) -> Vec<(&'static str, Option<bool>)> {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/ift/src/main.rs";
    let files = [(root.to_owned(), SOURCE.to_owned())];
    let user = UserCrate {
        name: "ift", dir: "/ift/", root, files: &files,
        edition: edition::PackageEdition::of_manifest(edition).expect("an edition"), panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).expect("fixture crate");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("body checker");
    let key_of = |name: &str| names.items(0).find_map(|(key, _, _)| (names.item_text(key)?.name == Some(name)).then_some(key)).expect(name);
    let mut out = Vec::new();
    for name in ["if_let_else", "if_let_then", "plain_cond", "plain_cond_twin"] {
        let (types, v) = world.check_borrows(BodyRef::Free(key_of(name)), alloc);
        println!("{edition} {name}: types clean {}, {v:?}", types.is_clean());
        assert!(types.is_clean(), "{edition} `{name}`'s types: {types:?}");
        let got = if v.refused.is_some() { None } else { Some(!v.findings.is_empty()) };
        out.push((name, got));
    }
    out
}

#[test]
fn an_if_conditions_temporaries_die_by_edition() {
    let new = verdicts("2024");
    let old = verdicts("2021");
    println!("2024: {new:?}\n2021: {old:?}");
    let want_new = [("if_let_else", Some(false)), ("if_let_then", Some(true)), ("plain_cond", Some(false)), ("plain_cond_twin", Some(true))];
    let want_old = [("if_let_else", Some(true)), ("if_let_then", Some(true)), ("plain_cond", Some(false)), ("plain_cond_twin", Some(true))];
    assert_eq!(new, want_new, "2024: the scrutinee's guard dies before `else`, a plain condition's before either arm");
    assert_eq!(old, want_old, "2021: an `if let` scrutinee's guard lives to the statement's end");
}
