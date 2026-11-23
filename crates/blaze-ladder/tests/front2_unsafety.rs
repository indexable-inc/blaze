//! E0133: what only `unsafe` code may do, refused outside it (blaze-typeck rules/unsafety.rs).
//!
//! The rule: the Reference, "Unsafety": "dereferencing a raw pointer; reading or writing a
//! mutable or unsafe external static variable; accessing a field of a union, other than to
//! assign to it; calling an unsafe function (including an intrinsic or foreign function);
//! calling a safe function marked with a `target_feature` from a function that does not have a
//! `target_feature` attribute enabling the same features", each legal only in an `unsafe`
//! block or an `unsafe fn`'s body. The exceptions it states: `&raw const *p` and `&raw const`
//! of a `static mut` are safe ("Raw borrow operators", "Mutable statics"), and writing a union
//! field is safe ("Unions").
//!
//! Every case is a pair: the refused control (`E0133` and rustc's words) and its accepted twin
//! (`clean`), the twin the same operation written inside `unsafe` or in a form the rule allows.
//! A check that refused everything fails the twins; one that refused nothing fails the controls.
//! It needs no toolchain: the program is `#![no_core]`.

use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::{BodyError, lower_fn};
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body, eval_len};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::{HeadError, Store};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::front2::types::Why;
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::NewFront;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

/// The lang items as tests/front2_body_const_paths.rs models them, `Copy` for the values the
/// cases read, and the items the cases use.
const PRELUDE: &str = r#"#![feature(no_core, lang_items, intrinsics)]
#![no_core]
#![no_main]

#[lang = "pointee_sized"]
trait PointeeSized {}

#[lang = "meta_sized"]
trait MetaSized: PointeeSized {}

#[lang = "sized"]
trait Sized: MetaSized {}

#[lang = "copy"]
trait Copy {}

impl Copy for u8 {}
impl Copy for bool {}

unsafe fn uf() {}
fn sf() {}

struct S;
impl S {
    unsafe fn um(&self) {}
    fn sm(&self) {}
}

#[rustc_intrinsic]
unsafe fn assume(b: bool);

#[target_feature(enable = "neon")]
fn tf() {}

static mut M: u8 = 0;
static I: u8 = 0;

unsafe extern "C" {
    static E: u8;
    safe static ES: u8;
    fn cfn();
    safe fn sfn();
}

union U {
    a: u8,
    b: u8,
}

struct P {
    f: u8,
}

macro_rules! deref {
    ($p:expr) => {
        *$p
    };
}

macro_rules! deref_inside {
    ($p:expr) => {
        unsafe { *$p }
    };
}
"#;

/// The most constants one fn's lowering may need evaluated before it converts.
const MOST_LENS: u32 = 8;

/// Each fn of `fns` in the crate `PRELUDE` + `text` of `edition`, lowered and checked in order
/// over one store: its name and outcome ("clean", "undecided" with what the check could not
/// decide, each diagnostic's code and words, or the failure).
fn check_fns(edition: &'static str, text: &str, fns: &[&str]) -> Vec<(String, String)> {
    let program = format!("{PRELUDE}{text}");
    let files = [SourceFile { path: "src/main.rs", text: &program }];
    let tc = Toolchain::none(Alloc::global());
    let new = NewFront { package: "ladder", edition, shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind).unwrap_or_else(|e| panic!("the program's texts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
    let mut front = Front2::new(&names, &frame, &files, Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut store = Store::new(front.index.lang("sized"), front.frame, Alloc::global());
    let (mut buckets, mut consts) = (None, Consts::default());
    let mut got = Vec::new();
    for &n in fns {
        let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[n] }, Alloc::global()).unwrap_or_else(|e| panic!("`{n}`: {e}"));
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
                        Ok(done) if !done.diags.is_empty() => {
                            let d: Vec<String> = done.diags.iter().map(|d| format!("{} at {:?}", d.code.code(), d.at)).collect();
                            format!("diags {d:?}")
                        }
                        Ok(done) if !done.findings.is_empty() => {
                            let f: Vec<String> = done.findings.iter().map(|f| format!("{} at {:?}", f.lint(), f.at())).collect();
                            format!("lint {f:?}")
                        }
                        Ok(done) if !done.undecided.is_empty() => {
                            let u: Vec<&str> = done.undecided.iter().map(|u| u.says()).collect();
                            format!("undecided {u:?}")
                        }
                        Ok(_) => "clean".to_string(),
                        Err(CheckFail::Diags { diags, .. }) => {
                            let d: Vec<String> = diags.iter().map(|(code, _, about)| format!("{} {about}", code.code())).collect();
                            d.join("; ")
                        }
                        Err(e) => format!("not checked: {e} ({e:?})"),
                    };
                }
            }
        };
        got.push((n.to_string(), outcome));
    }
    got
}

/// The cases of edition 2024: each fn's name and the outcome it must have.
const CASES: &str = r#"
fn call_unsafe() { uf() }
fn call_unsafe_ok() { unsafe { uf() } }
fn call_safe() { sf() }
unsafe fn call_in_unsafe_fn() { uf() }
unsafe fn call_in_unsafe_fn_block() { unsafe { uf() } }
unsafe fn deref_in_unsafe_fn(p: *const u8) -> u8 { *p }
fn method_unsafe(s: &S) { s.um() }
fn method_unsafe_ok(s: &S) { unsafe { s.um() } }
fn method_safe(s: &S) { s.sm() }
fn ptr_unsafe(f: unsafe fn()) { f() }
fn ptr_unsafe_ok(f: unsafe fn()) { unsafe { f() } }
fn ptr_safe(f: fn()) { f() }
fn intrinsic() { assume(true) }
fn intrinsic_ok() { unsafe { assume(true) } }
fn foreign() { cfn() }
fn foreign_ok() { unsafe { cfn() } }
fn foreign_safe() { sfn() }
fn feature() { tf() }
fn feature_ok() { unsafe { tf() } }
#[target_feature(enable = "neon")]
fn feature_from_feature() { tf() }
fn deref_read(p: *const u8) -> u8 { *p }
fn deref_read_ok(p: *const u8) -> u8 { unsafe { *p } }
fn deref_write(p: *mut u8) { *p = 1; }
fn deref_write_ok(p: *mut u8) { unsafe { *p = 1; } }
fn deref_borrow(p: *const u8) -> &'static u8 { &*p }
fn deref_field(p: *const P) -> u8 { (*p).f }
fn deref_raw(p: *const u8) -> *const u8 { &raw const *p }
fn deref_raw_field(p: *const P) -> *const u8 { &raw const (*p).f }
fn deref_reference(p: &u8) -> u8 { *p }
fn static_mut_read() -> u8 { M }
fn static_mut_write() { M = 1; }
fn static_mut_ok() -> u8 { unsafe { M } }
fn static_mut_raw() -> *const u8 { &raw const M }
fn static_plain() -> u8 { I }
fn static_extern() -> u8 { E }
fn static_extern_ok() -> u8 { unsafe { E } }
fn static_extern_raw() -> *const u8 { &raw const E }
fn static_extern_safe() -> u8 { ES }
fn union_read(u: U) -> u8 { u.a }
fn union_read_ok(u: U) -> u8 { unsafe { u.b } }
fn union_write(mut u: U) { u.a = 1; }
fn union_borrow(u: &U) -> &u8 { &u.a }
fn union_raw(u: &U) -> *const u8 { &raw const u.a }
fn union_pattern(u: U) -> u8 { let U { a } = u; a }
fn union_pattern_ok(u: U) -> u8 { unsafe { let U { a } = u; a } }
fn union_pattern_wild(u: U) { let U { a: _ } = u; }
fn union_pattern_two(u: U) { unsafe { let U { a: _, b: _ } = u; } }
fn macro_outside(p: *const u8) -> u8 { deref!(p) }
fn macro_inside(p: *const u8) -> u8 { deref_inside!(p) }
fn macro_in_block(p: *const u8) -> u8 { unsafe { deref!(p) } }
fn closure_outside(p: *const u8) { let _c = || *p; }
fn closure_inside(p: *const u8) { unsafe { let _c = || *p; } }
fn nested_closure_block(p: *const u8) { let _c = || unsafe { *p }; }
"#;

/// The cases' names and outcomes, in the order `CASES` writes them.
const WANT: &[(&str, &str)] = &[
    ("call_unsafe", "E0133 call to unsafe function `uf` is unsafe and requires unsafe block (E0133 RequiresUnsafe)"),
    ("call_unsafe_ok", "clean"),
    ("call_safe", "clean"),
    // `unsafe_op_in_unsafe_fn`, edition 2024: a lint finding (no error), at the call.
    ("call_in_unsafe_fn", "lint [\"unsafe-op-in-unsafe-fn at *"),
    ("call_in_unsafe_fn_block", "clean"),
    ("deref_in_unsafe_fn", "lint [\"unsafe-op-in-unsafe-fn at *"),
    ("method_unsafe", "E0133 call to unsafe function `um` is unsafe and requires unsafe block (E0133 RequiresUnsafe)"),
    ("method_unsafe_ok", "clean"),
    ("method_safe", "clean"),
    ("ptr_unsafe", "E0133 call to unsafe function is unsafe and requires unsafe block (E0133 RequiresUnsafe)"),
    ("ptr_unsafe_ok", "clean"),
    ("ptr_safe", "clean"),
    ("intrinsic", "E0133 call to unsafe function `assume` is unsafe and requires unsafe block (E0133 RequiresUnsafe)"),
    ("intrinsic_ok", "clean"),
    ("foreign", "E0133 call to unsafe function `cfn` is unsafe and requires unsafe block (E0133 RequiresUnsafe)"),
    ("foreign_ok", "clean"),
    ("foreign_safe", "clean"),
    ("feature", "E0133 call to function `tf` with `#[target_feature]` is unsafe and requires unsafe block (E0133 RequiresUnsafe)"),
    ("feature_ok", "clean"),
    (
        "feature_from_feature",
        "undecided [\"a call of a safe `#[target_feature]` fn from a body with its own `#[target_feature]`: whether the body enables every feature of the callee is not read (the world gives no feature sets)\"]",
    ),
    ("deref_read", "E0133 dereference of raw pointer is unsafe and requires unsafe block (E0133 RequiresUnsafe)"),
    ("deref_read_ok", "clean"),
    ("deref_write", "E0133 dereference of raw pointer is unsafe and requires unsafe block (E0133 RequiresUnsafe)"),
    ("deref_write_ok", "clean"),
    ("deref_borrow", "E0133 dereference of raw pointer is unsafe and requires unsafe block (E0133 RequiresUnsafe)"),
    ("deref_field", "E0133 dereference of raw pointer is unsafe and requires unsafe block (E0133 RequiresUnsafe)"),
    ("deref_raw", "clean"),
    ("deref_raw_field", "E0133 dereference of raw pointer is unsafe and requires unsafe block (E0133 RequiresUnsafe)"),
    ("deref_reference", "clean"),
    ("static_mut_read", "E0133 use of mutable static `M` is unsafe and requires unsafe block (E0133 RequiresUnsafe)"),
    ("static_mut_write", "E0133 use of mutable static `M` is unsafe and requires unsafe block (E0133 RequiresUnsafe)"),
    ("static_mut_ok", "clean"),
    ("static_mut_raw", "clean"),
    ("static_plain", "clean"),
    ("static_extern", "E0133 use of extern static `E` is unsafe and requires unsafe block (E0133 RequiresUnsafe)"),
    ("static_extern_ok", "clean"),
    ("static_extern_raw", "clean"),
    ("static_extern_safe", "clean"),
    ("union_read", "E0133 access to union field is unsafe and requires unsafe block (E0133 RequiresUnsafe)"),
    ("union_read_ok", "clean"),
    ("union_write", "clean"),
    ("union_borrow", "E0133 access to union field is unsafe and requires unsafe block (E0133 RequiresUnsafe)"),
    ("union_raw", "clean"),
    ("union_pattern", "E0133 access to union field is unsafe and requires unsafe block (E0133 RequiresUnsafe)"),
    ("union_pattern_ok", "clean"),
    ("union_pattern_wild", "clean"),
    ("union_pattern_two", "E0784 *"),
    ("macro_outside", "E0133 dereference of raw pointer is unsafe and requires unsafe block (E0133 RequiresUnsafe), at expression #1 (a unary operation; its range is shared by 2 nodes): `*p`"),
    ("macro_inside", "clean"),
    ("macro_in_block", "clean"),
    ("closure_outside", "E0133 dereference of raw pointer is unsafe and requires unsafe block (E0133 RequiresUnsafe)"),
    ("closure_inside", "clean"),
    ("nested_closure_block", "clean"),
];

/// Every case of edition 2024 has its outcome; every wrong row is named in the failure.
#[test]
fn unsafe_operations_need_unsafe() {
    let names: Vec<&str> = WANT.iter().map(|w| w.0).collect();
    let got = check_fns("2024", CASES, &names);
    let mut wrong = Vec::new();
    for ((n, outcome), (_, want)) in got.iter().zip(WANT) {
        // A want ending in `*` is a prefix: the words after the code are another rule's.
        let as_wanted = match want.strip_suffix('*') {
            Some(prefix) => outcome.starts_with(prefix),
            None => outcome == want,
        };
        if !as_wanted {
            wrong.push(format!("{n}: got {outcome:?}, want {want:?}"));
        }
    }
    println!("{} of {} cases as wanted", WANT.len() - wrong.len(), WANT.len());
    assert!(wrong.is_empty(),"{} of {} cases wrong:\n{}", wrong.len(), WANT.len(), wrong.join("\n"));
}

/// Before 2024 an `unsafe fn`'s body is itself unsafe context and the words say so
/// ("requires unsafe function or block"); the same call inside `unsafe` is clean.
#[test]
fn before_2024_the_words_name_the_unsafe_fn() {
    let text = "fn call_2021() { uf() }\nfn call_2021_ok() { unsafe { uf() } }\nunsafe fn in_unsafe_fn_2021() { uf() }\n";
    let got = check_fns("2021", text, &["call_2021", "call_2021_ok", "in_unsafe_fn_2021"]);
    println!("{got:?}");
    assert_eq!(
        got,
        vec![
            ("call_2021".to_string(), "E0133 call to unsafe function `uf` is unsafe and requires unsafe function or block (E0133 RequiresUnsafe)".to_string()),
            ("call_2021_ok".to_string(), "clean".to_string()),
            ("in_unsafe_fn_2021".to_string(), "clean".to_string()),
        ]
    );
}
