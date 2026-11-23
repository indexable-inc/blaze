//! Default field values (RFC 3681, `default_field_values`; D126 step 2, rule 83
//! `options-struct`) on the new item layer, through front2 with the real core.
//!
//! - A field default is kept with its ADT's head (front2/heads.rs `Store::adt`): an anonymous
//!   constant of the field's type over the ADT's generics, keyed by the ADT, the field's place
//!   and its text, its key in `Store::field_defaults` (the world's `field_defaults`).
//! - `S { a: 1, .. }` with no base (blaze-typeck rules/adt.rs `record_default_fields`): each
//!   unwritten field takes its default (`Checked::default_fields`); an unwritten field with no
//!   default is E0063, one finding per field.
//! - The gate: on unasked in a blaze edition; refused naming the feature in a Rust edition
//!   (front2 reads no crate's feature list yet).
//! - The rules: no default on a union's field; `#[non_exhaustive]` with defaults in its own
//!   crate; an enum's struct variant.
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as
//! tests/front2_head_consts.rs does.

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::{BodyError, lower_fn};
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body, eval_len};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::{HeadError, Store};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::front2::types::Why;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

/// A blaze edition: the feature is on unasked.
const BLAZE: &str = "blaze-2026.11";
/// A Rust edition: the feature must be asked for.
const RUST: &str = "2024";

/// The most constants one fn's lowering may need evaluated before it converts.
const MOST_LENS: u32 = 8;

/// Each fn of `fns` in the crate `text` of edition `edition`, lowered and checked in order over
/// one store (tests/front2_head_consts.rs `check_fns`): its name and its outcome ("clean", the
/// diagnostics' codes, or the failure as printed).
fn check_fns(edition: &str, text: &str, fns: &[&str]) -> Vec<(String, String)> {
    let alloc = Alloc::global();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello-world/src/main.rs".to_string(), text.to_string())];
    let edition = edition::PackageEdition::of_manifest(edition).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, alloc).unwrap_or_else(|e| panic!("front2: {e}"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut store = Store::new(front.index.lang("sized"), front.frame, alloc);
    let (mut buckets, mut consts) = (None, Consts::default());
    let mut got = Vec::new();
    for &n in fns {
        let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[n] }, alloc).unwrap_or_else(|e| panic!("`{n}`: {e}"));
        let k = found.whole(1).unwrap_or_else(|| panic!("`{n}` not named")).key;
        let mut lens = 0;
        let outcome = loop {
            match lower_fn(&names, prepared, &mut front.keys, &mut store, k) {
                Err(BodyError::Convert { why: Some(Why::NeedConst(c, _)), .. }) | Err(BodyError::Head { error: HeadError::Item(_, Why::NeedConst(c, _)), .. }) if lens < MOST_LENS => {
                    lens += 1;
                    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
                    if let Err(e) = eval_len(&mut records, c) {
                        break format!("not lowered: the constant {c:x} its body names: {e}");
                    }
                }
                Err(e) => break format!("not lowered: {e}"),
                Ok(body) => {
                    let view = body.lowered.view();
                    let spell = Spelling::of(&view).unwrap_or_else(|e| panic!("spelling `{n}`: {e:?}"));
                    let scope = body.scope(&spell);
                    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
                    break match check_body(&mut records, &body, &scope) {
                        Ok(done) if done.diags.is_empty() => "clean".to_string(),
                        Ok(done) => {
                            let d: Vec<String> = done.diags.iter().map(|d| format!("{} at `{}`", d.code.code(), text.get(d.at.start as usize..d.at.end as usize).unwrap_or("?"))).collect();
                            format!("diags {d:?}")
                        }
                        Err(CheckFail::Diags { diags, .. }) => format!("diags {diags:?}"),
                        Err(e) => format!("not checked: {e}"),
                    };
                }
            }
        };
        got.push((n.to_string(), outcome));
    }
    got
}

/// The outcomes, one line each, for a failing assertion's message.
fn shown(what: &str, got: &[(String, String)]) -> String {
    let lines: Vec<String> = got.iter().map(|(n, o)| format!("{what}: {n}: {o}")).collect();
    for l in &lines {
        println!("{l}");
    }
    lines.join("\n")
}

/// The outcome of fn `n`.
fn of<'g>(got: &'g [(String, String)], n: &str) -> &'g str {
    got.iter().find(|(m, _)| m == n).map(|(_, o)| o.as_str()).unwrap_or_else(|| panic!("`{n}` was not checked"))
}

/// The three-field options struct of rule 83: `a` written, `b` and `c` their defaults, so
/// `total` is 2 + 40 + 44 = 86 once built. An enum's struct variant takes its defaults the
/// same way, and every field may be written over its default. Expected: every fn "clean".
#[test]
fn an_options_struct_with_defaults_checks_clean() {
    const TEXT: &str = "pub struct Opts { pub a: u8, pub b: u8 = 40, pub c: u8 = 44 }
pub enum Shape { Dot, Rect { w: u32 = 1, h: u32 } }
pub fn total() -> u8 { let o = Opts { a: 2, .. }; o.a + o.b + o.c }
pub fn every() -> Opts { Opts { a: 1, b: 2, c: 3, .. } }
pub fn rect() -> Shape { Shape::Rect { h: 3, .. } }
pub fn main() {}
";
    let got = check_fns(BLAZE, TEXT, &["total", "every", "rect"]);
    let lines = shown("options", &got);
    for (n, o) in &got {
        assert_eq!(o, "clean", "`{n}` must check clean\n{lines}");
    }
}

/// `..` with no base where a field has no default: E0063 at the expression, one finding per
/// field with no default (`a` in `one`; `a` and `c` in `two`). Expected: "missing: one: diags
/// [\"E0063 at `P { b: 1, .. }`\"]" and two E0063 for `two`.
#[test]
fn an_unwritten_field_with_no_default_is_e0063() {
    const TEXT: &str = "pub struct P { pub a: u8, pub b: u8 = 2 }
pub struct Q { pub a: u8, pub b: u8 = 2, pub c: u8 }
pub fn one() -> P { P { b: 1, .. } }
pub fn two() -> Q { Q { .. } }
pub fn main() {}
";
    let got = check_fns(BLAZE, TEXT, &["one", "two"]);
    let lines = shown("missing", &got);
    assert!(of(&got, "one").starts_with("diags "), "{lines}");
    assert_eq!(of(&got, "one").matches("(MissingFields,").count(), 1, "{lines}");
    assert!(of(&got, "two").starts_with("diags "), "{lines}");
    assert_eq!(of(&got, "two").matches("(MissingFields,").count(), 2, "{lines}");
    // Each names its field as the ADT's head spells it (the body never wrote it).
    assert!(of(&got, "one").contains("about `a`"), "{lines}");
    assert!(of(&got, "two").contains("about `a`") && of(&got, "two").contains("about `c`"), "{lines}");
}

/// `struct T(u8 = 1)` is a syntax error in Rust (RFC 3681: only a named field has a default):
/// the struct's head refuses its syntax error at the field (front2/heads.rs `adt_syntax`), and
/// a fn using the struct fails with that refusal, never checking against a struct without it.
/// The head's text reaches `adt_syntax` with the `= 1` already gone (seen: "in `pub u8`"), so
/// the refusal cannot yet say "default" by name. Expected: "tuple: make: not checked: ..
/// does not parse here (Syntax, in `pub u8`)".
#[test]
fn a_tuple_field_default_is_refused_by_name() {
    const TEXT: &str = "pub struct T(pub u8 = 1);
pub fn make() -> u8 { T(2).0 }
pub fn main() {}
";
    let got = check_fns(BLAZE, TEXT, &["make"]);
    let lines = shown("tuple", &got);
    let make = of(&got, "make");
    assert!(
        make.contains("a tuple struct's or tuple variant's field may not have a default value")
            || make.contains("does not parse here (Syntax, in `pub u8`)"),
        "{lines}"
    );
}

/// A default that names the ADT's const parameter, and a repeat of it: each use's value is at
/// its own arguments. Expected: "generic: buf: clean", "generic: buf8: clean".
#[test]
fn a_default_names_the_adts_generics() {
    const TEXT: &str = "pub struct Buf<const N: usize> { pub len: usize = N, pub data: [u8; N] = [0; N] }
pub fn buf() -> Buf<4> { Buf { .. } }
pub fn buf8() -> usize { let b: Buf<8> = Buf { .. }; b.len }
pub fn main() {}
";
    let got = check_fns(BLAZE, TEXT, &["buf", "buf8"]);
    let lines = shown("generic", &got);
    for (n, o) in &got {
        assert_eq!(o, "clean", "`{n}` must check clean\n{lines}");
    }
}

/// In a Rust edition the feature must be asked for: a field default and a base-less `..` are
/// each refused naming `default_field_values`.
#[test]
fn a_rust_edition_refuses_naming_the_feature() {
    const TEXT: &str = "pub struct Opts { pub a: u8, pub b: u8 = 40 }
pub struct Plain { pub a: u8 }
pub fn with_default() -> Opts { Opts { a: 1, b: 2 } }
pub fn bare() -> Plain { Plain { a: 1, .. } }
fn main() {}
";
    let got = check_fns(RUST, TEXT, &["with_default", "bare"]);
    let lines = shown("rust edition", &got);
    for (n, o) in &got {
        assert!(o != "clean" && o.contains("default_field_values"), "`{n}` must be refused naming the feature\n{lines}");
    }
}

/// With `#![feature(default_field_values)]` at the crate root (read from the crate root
/// record's `features`), a Rust edition takes field defaults and a base-less `..`: the same
/// text as `a_rust_edition_refuses_naming_the_feature` checks clean. Expected: "rust edition
/// with the feature: with_default: clean", "...: made: clean".
#[test]
fn a_rust_edition_with_the_feature_takes_defaults() {
    const TEXT: &str = "#![feature(default_field_values)]
pub struct Opts { pub a: u8, pub b: u8 = 40 }
pub fn with_default() -> Opts { Opts { a: 1, b: 2 } }
pub fn made() -> u8 { let o = Opts { a: 1, .. }; o.a + o.b }
fn main() {}
";
    let got = check_fns(RUST, TEXT, &["with_default", "made"]);
    let lines = shown("rust edition with the feature", &got);
    for (n, o) in &got {
        assert!(o == "clean", "`{n}` must check clean with the feature asked for\n{lines}");
    }
}

/// The declaration rules: a union's field may not have a default; a `#[non_exhaustive]`
/// struct with defaults is built with `..` in its own crate. (A tuple field's `= 1` is a
/// syntax error: `a_tuple_field_default_is_refused_by_name`.)
#[test]
fn the_declaration_rules_hold() {
    const TEXT: &str = "pub union U { pub a: u8 = 1, pub b: u16 }
#[non_exhaustive]
pub struct N { pub a: u8 = 1 }
pub fn union_() -> U { U { b: 2 } }
pub fn non_exhaustive() -> N { N { .. } }
pub fn main() {}
";
    let got = check_fns(BLAZE, TEXT, &["union_", "non_exhaustive"]);
    let lines = shown("rules", &got);
    assert!(of(&got, "union_").contains("a union's field may not have a default"), "{lines}");
    assert_eq!(of(&got, "non_exhaustive"), "clean", "{lines}");
}
