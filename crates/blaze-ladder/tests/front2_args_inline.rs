//! `args-inline` (AGENTS.md rule 83 `options-struct`, decision D126), the first typed lint, on
//! the new item layer through front2 with the real core:
//!
//! - The mark: `#[blaze::args]` on a struct is kept on its ADT's head (front2/heads.rs
//!   `AdtRow::args`), in its head record (head_record.rs, version 7: so an args struct of ANOTHER
//!   crate, whose head is read from its record, is known), and answered by the world as
//!   `TypeAttrs::ARGS`.
//! - The rule (blaze-typeck rules/args_inline.rs): a value of an args struct is only a struct
//!   literal directly an argument of a call or method call. Each misuse is one finding naming the
//!   struct: a literal anywhere else (a), a binding of its type that is no parameter (b), a
//!   parameter of it passed on whole (c), a body whose result is it.
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_project.rs.

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::head_record::{HeadAt, HeadRef, decode, encode, from_store};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const EDITION: &str = "blaze-2026.11";

/// An options struct and its takers; one fn per case.
const TEXT: &str = "#[blaze::args]
pub struct MapOpts { pub cap: u8, pub shards: u8 }
pub struct Plain { pub a: u8 }
pub struct Map { pub cap: u8 }
impl Map {
    pub fn new(opts: MapOpts) -> Map { Map { cap: opts.cap } }
    pub fn grow(&self, opts: MapOpts) -> u8 { self.cap + opts.shards }
}
pub fn take(opts: MapOpts) -> u8 { opts.cap }
pub fn pair(a: u8, b: u8) -> u8 { a + b }
pub fn as_call_arg() -> Map { Map::new(MapOpts { cap: 1, shards: 2 }) }
pub fn as_method_arg(m: &Map) -> u8 { m.grow(MapOpts { cap: 1, shards: 2 }) }
pub fn nested() -> u8 { pair(take(MapOpts { cap: 1, shards: 2 }), 3) }
pub fn parens() -> u8 { take((MapOpts { cap: 1, shards: 2 })) }
pub fn rebuilt(opts: MapOpts) -> u8 { take(MapOpts { cap: opts.cap, shards: opts.shards + 1 }) }
pub fn unmarked(p: Plain) -> u8 { let q = Plain { a: p.a }; let r = q; r.a }
pub fn in_arm(x: u8) -> u8 { take(match x { _ => MapOpts { cap: x, shards: 1 } }) }
pub fn closure_result() -> u8 { let f = || MapOpts { cap: 1, shards: 2 }; take(f()) }
pub fn bound(opts: MapOpts) -> u8 { let o = opts; o.cap }
pub fn forwarded(opts: MapOpts) -> u8 { take(opts) }
pub fn forwarded_to_method(m: &Map, opts: MapOpts) -> u8 { m.grow(opts) }
pub fn helper() -> MapOpts { MapOpts { cap: 1, shards: 2 } }
fn main() {}
";

/// What each finding says after the struct's name (blaze-typeck `ArgsMisuse::says`).
const LITERAL: &str = "as a literal that is not directly an argument";
const BOUND: &str = "bound to a name";
const FORWARDED: &str = "passed on whole";
const RESULT: &str = "this body's result";

fn toolchain() -> Toolchain {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"))
}

/// Each body of `TEXT`, checked: its name, then each finding's lint, words and the text its
/// range covers in the item; or the body's refusal as printed.
fn findings() -> Vec<(String, Result<Vec<(String, String, String)>, String>)> {
    let tc = toolchain();
    let files = [("/hello-world/src/main.rs".to_string(), TEXT.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: TEXT }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command's frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let bodies = world.bodies(Alloc::global());
    let mut out = Vec::new();
    for (b, _) in bodies.iter() {
        let (name, item) = match *b {
            BodyRef::Free(k) => (names.item_text(k).and_then(|t| t.name).unwrap_or("?").to_string(), k),
            BodyRef::Member { container, name, .. } => (name.to_string(), container),
        };
        let v = world.check(*b, Alloc::global());
        let got = if !v.is_clean() {
            Err(format!("{v:?}"))
        } else {
            let text = names.item_text(item).map(|t| t.text).unwrap_or("");
            Ok(v.findings.iter().map(|f| (f.lint.to_string(), f.text.as_str().to_string(), text.get(f.start as usize..f.end as usize).unwrap_or("?").to_string())).collect())
        };
        out.push((name, got));
    }
    for (n, got) in &out {
        println!("args-inline: {n}: {got:?}");
    }
    out
}

fn of<'g>(got: &'g [(String, Result<Vec<(String, String, String)>, String>)], n: &str) -> &'g [(String, String, String)] {
    match got.iter().find(|(m, _)| m == n) {
        Some((_, Ok(f))) => f,
        Some((_, Err(e))) => panic!("`{n}` was not checked clean: {e}"),
        None => panic!("`{n}` was not listed"),
    }
}

/// Every finding is `args-inline` and names `MapOpts`; `n` has exactly the findings `kinds`
/// says, in range order.
fn has(got: &[(String, Result<Vec<(String, String, String)>, String>)], n: &str, kinds: &[&str]) {
    let f = of(got, n);
    assert_eq!(f.len(), kinds.len(), "`{n}`: {f:?}");
    for ((lint, words, _), kind) in f.iter().zip(kinds) {
        assert_eq!(lint, "args-inline", "`{n}`: {f:?}");
        assert!(words.contains("`MapOpts`") || words.contains("::MapOpts`"), "`{n}` names the struct: {words}");
        assert!(words.contains(kind), "`{n}`: {kind:?} in {words}");
    }
}

/// The literal directly in a call, in a method call, nested in another call's argument list, in
/// parentheses, and the taker writing a new literal from its parameter's fields: clean. An
/// unmarked struct is never flagged, bound or not. Expected: each listed fn has no finding.
#[test]
fn the_literal_in_the_call_is_clean() {
    let got = findings();
    for n in ["new", "grow", "take", "as_call_arg", "as_method_arg", "nested", "parens", "rebuilt", "unmarked", "main"] {
        assert!(of(&got, n).is_empty(), "`{n}` must have no finding: {:?}", of(&got, n));
    }
}

/// (a) a literal anywhere else, (b) a binding of the type that is no parameter, (c) a parameter
/// passed on whole to a call or a method call; a helper returning one is a literal and a result.
/// Expected: one finding each, naming `MapOpts`; `helper` two.
#[test]
fn each_misuse_is_one_finding_naming_the_struct() {
    let got = findings();
    has(&got, "in_arm", &[LITERAL]);
    has(&got, "closure_result", &[LITERAL]);
    has(&got, "bound", &[BOUND]);
    has(&got, "forwarded", &[FORWARDED]);
    has(&got, "forwarded_to_method", &[FORWARDED]);
    let helper = of(&got, "helper");
    assert_eq!(helper.len(), 2, "`helper`: {helper:?}");
    assert!(helper.iter().any(|(_, w, _)| w.contains(LITERAL)) && helper.iter().any(|(_, w, _)| w.contains(RESULT)), "`helper`: {helper:?}");
    // The literal's range covers the literal (rule 71: item-relative).
    let (_, _, covered) = &of(&got, "in_arm")[0];
    assert!(covered.starts_with("MapOpts {"), "the range covers {covered:?}");
    let (_, _, covered) = &of(&got, "forwarded")[0];
    assert_eq!(covered, "opts", "the range covers {covered:?}");
}

/// The mark is in the ADT's head and its record: a head read back from its record (an ADT of
/// another crate is read so) keeps it, and an unmarked struct's record has none. Expected:
/// `MapOpts` true, `Plain` false, both after a round trip.
#[test]
fn the_mark_survives_the_head_record() {
    let tc = toolchain();
    let files = [("/hello-world/src/main.rs".to_string(), TEXT.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: TEXT }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command's frame");
    let mut front = Front2::new(&names, &frame, &sources, Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, Alloc::global());
    for (n, marked) in [("MapOpts", true), ("Plain", false)] {
        let found = names::path(&names, names.root(0), Namespace::Types, Written { kind: PathKind::Plain, segments: &[n] }, Alloc::global()).unwrap_or_else(|e| panic!("`{n}`: {e}"));
        let k = found.whole(1).unwrap_or_else(|| panic!("`{n}` not named")).key;
        store.ensure(&names, &mut front.keys, Want::Adt(k)).unwrap_or_else(|e| panic!("`{n}`'s head: {e:?}"));
        let head = from_store(&store, HeadAt::Adt(k)).unwrap_or_else(|e| panic!("`{n}`: {e}"));
        let HeadRef::Adt { row, .. } = head else { panic!("`{n}` is no ADT head") };
        assert_eq!(row.args, marked, "`{n}`'s made head");
        let bytes = encode(head, store.tys.view(), Alloc::global()).unwrap_or_else(|e| panic!("`{n}`: not encoded: {e}"));
        let text = head.text();
        let decoded = decode(&bytes, text, &store.tys, Alloc::global()).unwrap_or_else(|e| panic!("`{n}`: not decoded: {e}"));
        let HeadRef::Adt { row, .. } = decoded.as_ref() else { panic!("`{n}` decoded as no ADT head") };
        assert_eq!(row.args, marked, "`{n}`'s head read back from its record");
        println!("args-inline: `{n}` marked {marked}, kept through its head record");
    }
}
