//! `as_cast` per cast class: what each `as` is found as, the text its fix writes, that the
//! fixed crate has no finding left with a fix (the fix is idempotent), and that the casts with
//! no fix stay found and say why. Then `truncating_cast` over the fixed text: it lists the
//! wrapping operations the fix wrote, and nothing else.
//!
//! That each operation the fix writes has the value of the `as` it replaces, on Hegel's draws
//! and on every boundary value, is blaze-cast's own tests (crates/blaze-cast/src/tests.rs): one
//! per function and one per composition written here.

use base_db::FileId;
use blaze_hash::HashMap;
use hir_expand::fixture;
use std::alloc::Global;

use crate::{
    check::{Setup, check},
    conf::Conf,
    fix::fix,
    level::CommandLine,
    tests::{lint, main_crate},
};

/// core as the pinned nightly has it for the fixes, and a blaze-cast of the functions used.
/// Each has the body blaze-cast's has, the `as` it stands for: the fixed `WORDS` is evaluated,
/// and a `loop {}` there never ends (const eval's step bound refuses it).
const CRATES: &str = r#"
//- minicore: sized, copy, clone, fn, from, int_cast, coerce_unsized, unsize
//- /blaze_cast/lib.rs crate:blaze_cast
pub const fn usize_from_u32(x: u32) -> usize { x as usize }
pub const fn u64_from_usize(x: usize) -> u64 { x as u64 }
pub const fn u32_truncated_from_usize(x: usize) -> u32 { x as u32 }
pub const fn usize_truncated_from_u64(x: u64) -> usize { x as usize }
pub const fn u8_from_bool(x: bool) -> u8 { x as u8 }
pub const fn f64_from_f32(x: f32) -> f64 { x as f64 }
pub const fn f64_rounded_from_u64(x: u64) -> f64 { x as f64 }
pub const fn f32_rounded_from_f64(x: f64) -> f32 { x as f32 }
pub const fn i32_saturated_from_f64(x: f64) -> i32 { x as i32 }
pub const fn u8_saturated_from_f32(x: f32) -> u8 { x as u8 }
"#;

const MAIN: &str = "//- /src/lib.rs crate:main deps:blaze_cast\n#![feature(integer_widen_truncate)]\n";

/// One function per cast class: its `as`, and what the fix writes in its place.
const CLASSES: &[(&str, &str)] = &[
    // The same type: the operand; an unsuffixed literal gets the suffix.
    ("pub fn same(x: u32) -> u32 { x as u32 }", "pub fn same(x: u32) -> u32 { x }"),
    ("pub fn literal() -> u64 { 5 as u64 }", "pub fn literal() -> u64 { 5_u64 }"),
    // Lossless: std's `From`.
    ("pub fn widen(x: u8) -> u32 { x as u32 }", "pub fn widen(x: u8) -> u32 { u32::from(x) }"),
    ("pub fn signed(x: u16) -> i64 { x as i64 }", "pub fn signed(x: u16) -> i64 { i64::from(x) }"),
    // Lossless on every 32- and 64-bit target, which std doesn't promise: blaze-cast.
    (
        "pub fn index(x: u32) -> usize { x as usize }",
        "pub fn index(x: u32) -> usize { blaze_cast::usize_from_u32(x) }",
    ),
    (
        "pub fn length(n: usize) -> u64 { n as u64 }",
        "pub fn length(n: usize) -> u64 { blaze_cast::u64_from_usize(n) }",
    ),
    // Narrowing: the low bits, as `as` keeps them.
    ("pub fn narrow(x: u64) -> u32 { x as u32 }", "pub fn narrow(x: u64) -> u32 { x.truncate::<u32>() }"),
    (
        "pub fn short(n: usize) -> u32 { n as u32 }",
        "pub fn short(n: usize) -> u32 { blaze_cast::u32_truncated_from_usize(n) }",
    ),
    (
        "pub fn offset(x: u64) -> usize { x as usize }",
        "pub fn offset(x: u64) -> usize { blaze_cast::usize_truncated_from_u64(x) }",
    ),
    // A sign change; a sign change and a width change, the width first in the source's sign.
    ("pub fn sign(x: i32) -> u32 { x as u32 }", "pub fn sign(x: i32) -> u32 { x.cast_unsigned() }"),
    (
        "pub fn mixed(x: u64) -> i32 { x as i32 }",
        "pub fn mixed(x: u64) -> i32 { x.truncate::<u32>().cast_signed() }",
    ),
    (
        "pub fn extend(x: i32) -> u64 { x as u64 }",
        "pub fn extend(x: i32) -> u64 { i64::from(x).cast_unsigned() }",
    ),
    // `bool` and `char`.
    ("pub fn flag(b: bool) -> u8 { b as u8 }", "pub fn flag(b: bool) -> u8 { u8::from(b) }"),
    ("pub fn code(c: char) -> u32 { c as u32 }", "pub fn code(c: char) -> u32 { u32::from(c) }"),
    (
        "pub fn low(c: char) -> u8 { c as u8 }",
        "pub fn low(c: char) -> u8 { u32::from(c).truncate::<u8>() }",
    ),
    ("pub fn letter(x: u8) -> char { x as char }", "pub fn letter(x: u8) -> char { char::from(x) }"),
    // Floats: exact, rounded, saturated.
    ("pub fn exact(x: u32) -> f64 { x as f64 }", "pub fn exact(x: u32) -> f64 { f64::from(x) }"),
    (
        "pub fn ratio(x: u64) -> f64 { x as f64 }",
        "pub fn ratio(x: u64) -> f64 { blaze_cast::f64_rounded_from_u64(x) }",
    ),
    (
        "pub fn whole(x: f64) -> i32 { x as i32 }",
        "pub fn whole(x: f64) -> i32 { blaze_cast::i32_saturated_from_f64(x) }",
    ),
    ("pub fn wider(x: f32) -> f64 { x as f64 }", "pub fn wider(x: f32) -> f64 { f64::from(x) }"),
    (
        "pub fn single(x: f64) -> f32 { x as f32 }",
        "pub fn single(x: f64) -> f32 { blaze_cast::f32_rounded_from_f64(x) }",
    ),
    // Pointers.
    (
        "pub fn erase(p: *const u32) -> *const u8 { p as *const u8 }",
        "pub fn erase(p: *const u32) -> *const u8 { p.cast::<u8>() }",
    ),
    (
        "pub fn writable(p: *const u8) -> *mut u8 { p as *mut u8 }",
        "pub fn writable(p: *const u8) -> *mut u8 { p.cast_mut() }",
    ),
    (
        "pub fn borrowed(r: &u32) -> *const u32 { r as *const u32 }",
        "pub fn borrowed(r: &u32) -> *const u32 { core::ptr::from_ref(r) }",
    ),
    (
        "pub fn bytes(r: &mut u32) -> *mut u8 { r as *mut u32 as *mut u8 }",
        "pub fn bytes(r: &mut u32) -> *mut u8 { core::ptr::from_mut(r).cast::<u8>() }",
    ),
    (
        "pub fn address(p: *const u8) -> usize { p as usize }",
        "pub fn address(p: *const u8) -> usize { p.expose_provenance() }",
    ),
    (
        "pub fn pointer(a: usize) -> *const u8 { a as *const u8 }",
        "pub fn pointer(a: usize) -> *const u8 { core::ptr::with_exposed_provenance::<u8>(a) }",
    ),
    // Nested casts, fixed at once; parentheses kept where a method needs them, dropped where
    // a call replaces the cast inside them.
    (
        "pub fn nested(x: u8) -> u64 { (x as u32) as u64 }",
        "pub fn nested(x: u8) -> u64 { u64::from(u32::from(x)) }",
    ),
    (
        "pub fn shifted(x: u32) -> i32 { (x >> 1) as i32 }",
        "pub fn shifted(x: u32) -> i32 { (x >> 1).cast_signed() }",
    ),
    (
        "pub fn counted(x: u8) -> u32 { (x as u32).count() }",
        "pub fn counted(x: u8) -> u32 { u32::from(x).count() }",
    ),
    // `as _`: the inferred type, written out.
    (
        "pub fn inferred(x: u8) -> u32 { let y: u32 = x as _; y }",
        "pub fn inferred(x: u8) -> u32 { let y: u32 = u32::from(x); y }",
    ),
    // A const context: no trait functions, so `widen` and blaze-cast's const twins.
    (
        "pub const fn size(x: u8) -> u64 { x as u64 }",
        "pub const fn size(x: u8) -> u64 { x.widen::<u64>() }",
    ),
    (
        "pub const fn bit(b: bool) -> u8 { b as u8 }",
        "pub const fn bit(b: bool) -> u8 { blaze_cast::u8_from_bool(b) }",
    ),
    (
        "pub const fn precise(x: f32) -> f64 { x as f64 }",
        "pub const fn precise(x: f32) -> f64 { blaze_cast::f64_from_f32(x) }",
    ),
    (
        "pub const WORDS: usize = 4_u32 as usize;",
        "pub const WORDS: usize = blaze_cast::usize_from_u32(4_u32);",
    ),
    // A coercion: the type moves onto the `let`.
    (
        "pub fn erased() -> u32 { let f = id as fn(u32) -> u32; f(1) }",
        "pub fn erased() -> u32 { let f: fn(u32) -> u32 = id; f(1) }",
    ),
];

/// The casts that have no fix, each with what its note says.
const NO_FIX: &[(&str, &str)] = &[
    ("pub fn discriminant(e: Kind) -> u8 { e as u8 }", "fieldless enum's discriminant"),
    ("pub fn function() -> usize { id as usize }", "a function's address"),
];

/// The items the classes use.
const SHARED: &str = r#"
pub fn id(x: u32) -> u32 { x }
pub enum Kind { A, B }
pub trait Count { fn count(self) -> u32; }
impl Count for u32 { fn count(self) -> u32 { self } }
"#;

fn source(rows: &[(&str, &str)], after: bool) -> String {
    let mut out = String::from(SHARED);
    for (before, fixed) in rows {
        out.push_str(if after { fixed } else { before });
        out.push('\n');
    }
    out
}

fn fixture_of(main: &str) -> String {
    format!("{}{MAIN}{main}", CRATES.trim_start_matches('\n'))
}

/// What `blaze::as_cast` alone says of `main`.
fn found(main: &str) -> String {
    lint(&fixture_of(main), "-A clippy::all --warn=blaze::as_cast")
}

/// `main` after `--fix` with `lints` on, and what `blaze::as_cast` and
/// `blaze::truncating_cast` then say of it.
fn fixed(main: &str) -> (String, String) {
    let fx = fixture::db(&fixture_of(main));
    let file = *fx.files().last().expect("src/lib.rs");
    let mut db = fx.into_db();
    let krate = blaze_query::db::attach(&db, || main_crate(&db));
    let file = file.file_id(&db);
    let mut names: HashMap<FileId, String, Global> = HashMap::new_in(Global);
    names.insert(file, "src/lib.rs".to_owned());
    let name_of = |file: FileId| names.get(&file).cloned();
    let setup = |flags: &str| Setup {
        command_line: CommandLine::parse(&flags.split_whitespace().map(str::to_owned).collect::<Vec<_>>())
            .expect("the flags read"),
        conf: Conf::default(),
    };
    let fixing = setup("-A clippy::all --warn=blaze::as_cast");
    let made = fix(&mut db, krate, &fixing, &name_of).unwrap_or_else(|error| panic!("{error}"));
    assert!(made.applied > 0, "the fix made a suggestion");
    let after = db.file_text(file).text(&db).to_string();
    let listing = setup("-A clippy::all --warn=blaze::as_cast --warn=blaze::truncating_cast");
    let left = blaze_query::db::attach(&db, || {
        let found = check(&db, krate, &listing, &name_of).expect("the file is named");
        found.iter().map(|it| format!("{}\n", it.diagnostic.message)).collect::<String>()
    });
    (after, left)
}

/// Every cast is found once, with both types; one that can lose says what.
#[test]
fn every_cast_is_found_with_its_types() {
    let out = found(&source(CLASSES, false));
    let casts = CLASSES.iter().map(|(it, _)| it.matches(" as ").count()).sum::<usize>();
    assert_eq!(out.matches("this `as` in").count(), casts, "{out}");
    assert!(out.contains("this `as` in `fn widen` casts `u8` to `u32`"), "{out}");
    assert!(out.contains("this `as` in `fn narrow` casts `u64` to `u32`, which drops the high bits"), "{out}");
    assert!(out.contains("casts `i32` to `u32`, which turns a negative value into a large positive one"), "{out}");
    assert!(out.contains("casts `f64` to `i32`, which drops the fraction"), "{out}");
    assert!(out.contains("casts `*const u8` to `usize`, which keeps the address"), "{out}");
}

/// Each class's fix writes the operation it names, and the fixed crate has no cast left: the
/// fix settles, and run again would find nothing to fix.
#[test]
fn each_class_is_fixed_to_its_operation_and_the_fix_is_idempotent() {
    let (after, left) = fixed(&source(CLASSES, false));
    for (before, fixed) in CLASSES {
        assert!(after.contains(fixed), "`{before}` is fixed to `{fixed}`:\n{after}");
    }
    assert!(!left.contains("this `as` in"), "the fixed crate has no cast left:\n{after}\n{left}");
}

/// What the fix wrote that wraps is what `truncating_cast` lists: `truncate`, `cast_signed`,
/// `cast_unsigned` and blaze-cast's `_truncated_from_`, once each where written.
#[test]
fn truncating_cast_lists_the_wrapping_operations_the_fix_wrote() {
    let (_, left) = fixed(&source(CLASSES, false));
    let wrapping = CLASSES
        .iter()
        .map(|(_, it)| {
            ["truncate::<", "cast_signed()", "cast_unsigned()", "_truncated_from_"]
                .iter()
                .map(|op| it.matches(op).count())
                .sum::<usize>()
        })
        .sum::<usize>();
    let listed = left.lines().filter(|it| it.starts_with("this `")).count();
    assert_eq!(listed, wrapping, "{left}");
}

/// A cast with no fix stays found, and its note says why.
#[test]
fn a_cast_with_no_fix_says_why() {
    let out = found(&source(NO_FIX, false));
    assert_eq!(out.matches("this `as` in").count(), NO_FIX.len(), "{out}");
    for (_, why) in NO_FIX {
        assert!(out.contains(why), "{why}: {out}");
    }
}

/// Without `integer_widen_truncate` at the crate root a narrowing has no fix and says so; a
/// `From` still has one.
#[test]
fn a_narrowing_without_the_feature_has_no_fix() {
    let main = format!("{SHARED}pub fn narrow(x: u64) -> u32 {{ x as u32 }}\n");
    let fixture = format!("{}//- /src/lib.rs crate:main deps:blaze_cast\n{main}", CRATES.trim_start_matches('\n'));
    let out = lint(&fixture, "-A clippy::all --warn=blaze::as_cast");
    assert!(out.contains("integer_widen_truncate"), "{out}");
}
