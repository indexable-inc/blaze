//! READ LISTS AS FACTS (drive.md, "End state: the whole pipeline on the command", piece 8):
//! every body's verdict holds the facts its check asked of the world, each with the digest of
//! the answer it got, sorted by fact (front2 reads.rs).
//!
//! The program has a fn `a::f` and three bodies: `calls` names `a::f`, `quiet` names nothing
//! of `a`, `wrong` names `a::f` and has a type error. What must hold:
//!
//! - only a body that names `a::f` lists `f`'s head (`FactKind::Head` about `f`'s key);
//! - `f`'s signature changed (`x: u8` to `x: u16`; its body and every other item as they
//!   were): `calls`' entry for `f`'s head has another answer digest, and `quiet`'s whole list
//!   is the same, row for row (the cause removed, rule 23: the edit is to `f`'s signature
//!   alone, and the one list that does not name `f` does not move);
//! - a body with a type error still has its whole list (`Reach::Whole`, with `f`'s head in it)
//!   and a digest;
//! - every list is sorted by its facts' bytes with no fact twice, and a check run again gives
//!   the same list.
//!
//! Expected lines:
//! `READS: calls lists f's head (answer <16 hex>), quiet does not (<n> facts), wrong lists it with 1 diagnostic`
//! `READS: f's signature changed: calls' answer for f's head <16 hex> -> <16 hex>; quiet's <n> facts unchanged`

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::front2::project::{BodyRef, BodyVerdict, ProjectWorld};
use blaze_ladder::front2::reads::{BodyReads, FactKind, Reach, Read};
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::resolve::{Ns, Paths};
use blaze_own::Alloc;

const EDITION: &str = "blaze-2026.11";

/// The program with `f`'s parameter written as `X`.
const PROGRAM: &str = "mod a {
    pub fn f(x: X) -> u8 {
        1
    }
}

fn calls() -> u8 {
    a::f(1)
}

fn quiet() -> u8 {
    2
}

fn wrong() -> u8 {
    let x: u8 = \"a\";
    a::f(1)
}

fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    io.need_out()?.line(cx, \"Hello, world!\")?;
    Ok(())
}
";

/// What one version of the program gave.
struct Lists {
    /// `f`'s checker key.
    f: u128,
    calls: Vec<Read>,
    quiet: Vec<Read>,
    wrong: Vec<Read>,
    wrong_diagnostics: usize,
    wrong_has_digest: bool,
}

/// A whole list's rows, checked sorted by fact bytes with no fact twice.
fn rows_of(name: &str, verdict: &BodyVerdict<'_>) -> Vec<Read> {
    let list: &BodyReads<'_> = verdict.facts.as_ref().unwrap_or_else(|e| panic!("`{name}`'s read list: {e}"));
    assert_eq!(list.reach, Reach::Whole, "`{name}`'s check finished, so its list is whole: {verdict:?}");
    assert!(list.rows.windows(2).all(|w| w[0].fact.bytes() < w[1].fact.bytes()), "`{name}`'s list is sorted by fact with no fact twice: {:?}", list.rows);
    assert!(list.rows.iter().all(|row| row.answer.is_some()), "`{name}`'s whole list has an answer for every fact: {:?}", list.rows);
    assert!(!list.rows.is_empty(), "`{name}` asked the world for something");
    list.rows.iter().copied().collect()
}

fn lists(param: &str) -> Lists {
    let text = PROGRAM.replace("x: X", &format!("x: {param}"));
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/reads/src/main.rs".to_string(), text.clone())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "reads", dir: "/reads/", root: "/reads/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: &text }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command's frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let bodies = world.bodies(Alloc::global());
    let body = |name: &str| {
        bodies
            .iter()
            .map(|(b, _)| *b)
            .find(|b| matches!(b, BodyRef::Free(k) if names.item_text(*k).and_then(|t| t.name) == Some(name)))
            .unwrap_or_else(|| panic!("`{name}` is a listed body"))
    };
    let f = Paths { names: &names }.path(names.root(0), false, &["a", "f"], Ns::Value).unwrap_or_else(|e| panic!("`a::f` from the crate root: {e}")).key;

    let calls = world.check(body("calls"), Alloc::global());
    assert!(calls.is_clean(), "calls: {calls:?}");
    let quiet = world.check(body("quiet"), Alloc::global());
    assert!(quiet.is_clean(), "quiet: {quiet:?}");
    let wrong = world.check(body("wrong"), Alloc::global());
    assert!(wrong.gaps.is_empty() && wrong.refused.is_none(), "wrong: {wrong:?}");
    // The same body checked again, after others: the same list (a list does not depend on
    // what was checked before it).
    let again = world.check(body("calls"), Alloc::global());
    assert_eq!(rows_of("calls", &calls), rows_of("calls", &again), "calls checked again lists the same facts");
    assert!(calls.reads.is_some() && calls.reads == again.reads, "calls' digest is its list's: {:?} then {:?}", calls.reads, again.reads);
    Lists {
        f: checker_of(f).0,
        calls: rows_of("calls", &calls),
        quiet: rows_of("quiet", &quiet),
        wrong: rows_of("wrong", &wrong),
        wrong_diagnostics: wrong.diagnostics.len(),
        wrong_has_digest: wrong.reads.is_some(),
    }
}

/// The answer digest of the head of `f` in `rows`, if listed.
fn head_of(rows: &[Read], f: u128) -> Option<u64> {
    rows.iter().find(|row| row.fact.kind == FactKind::Head && row.fact.key == f).and_then(|row| row.answer)
}

#[test]
fn only_the_body_naming_f_lists_its_head() {
    let before = lists("u8");
    let calls_head = head_of(&before.calls, before.f).unwrap_or_else(|| panic!("`calls` names `a::f` and does not list its head: {:?}", before.calls));
    assert!(before.quiet.iter().all(|row| row.fact.key != before.f), "`quiet` names nothing of `a` and lists a fact about `f`: {:?}", before.quiet);
    assert_eq!(before.wrong_diagnostics, 1, "`wrong` has its one type error");
    assert!(before.wrong_has_digest, "a body with a diagnostic has its list's digest");
    assert!(head_of(&before.wrong, before.f).is_some(), "`wrong` has a type error and still lists `f`'s head: {:?}", before.wrong);
    println!("READS: calls lists f's head (answer {calls_head:016x}), quiet does not ({} facts), wrong lists it with {} diagnostic", before.quiet.len(), before.wrong_diagnostics);

    // The cause moved: `f`'s signature, and nothing else.
    let after = lists("u16");
    let moved = head_of(&after.calls, after.f).unwrap_or_else(|| panic!("`calls` lists `f`'s head after the edit: {:?}", after.calls));
    assert_ne!(calls_head, moved, "`f`'s signature changed and the digest of its head in `calls`' list did not");
    assert_eq!(before.quiet, after.quiet, "`f`'s signature changed and `quiet`'s list moved");
    println!("READS: f's signature changed: calls' answer for f's head {calls_head:016x} -> {moved:016x}; quiet's {} facts unchanged", after.quiet.len());
}
