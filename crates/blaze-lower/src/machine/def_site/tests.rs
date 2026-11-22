//! A body-local `macro_rules!`'s template sees the locals and labels in scope at its
//! definition, and only those; a name passed in resolves where the caller wrote it.
//!
//! The rule: the Reference, "Macros By Example", Hygiene: "Macros by example have mixed-site
//! hygiene. This means that loop labels, block labels, and local variables are looked up at
//! the macro definition site while other symbols are looked up at the macro invocation site."
//! The case: tempo's `zone_checker::l1::events::decode_portal_event` (census 14: 38 false
//! E0425 at `log` and `block` in 19 `ignored!` calls), whose `macro_rules! ignored` is written
//! in the fn's body and names the fn's parameters `log` and `block`.
//!
//! Run: `LANECHECK_WAIT=540 bash /Volumes/Projects/blaze-int-1112-notes/lanecheck.sh blaze-lower test def_site`.
//! Expected: `test machine::def_site::tests::<name> ... ok` for each of the five.
//! The control: with `path_expr` back on `self.lookup(self.local_key(name))` (the wiring's
//! Edit 6 out), `a_template_sees_the_locals_at_its_definition` fails with "the template's `log`
//! and `block` are the parameters" and an empty left side: the template's names were keyed
//! with the expansion's hygiene and never met the parameters' (hygiene 0).

use std::vec;
use std::vec::Vec as StdVec;

use blaze_own::Alloc;

use crate::by_name::ByName;
use crate::columns::View;
use crate::expand_tests::{Host, Unix, at, dump_of};
use crate::form::*;
use crate::{Edition, Lowered, lower_body_expanding};

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

/// `text`'s body lowered with no macro but the body's own `macro_rules!`; asserts every form
/// handled.
#[track_caller]
fn lowered(text: &'static str) -> Lowered<'static, 'static> {
    let mut expander = ByName::new(&[], at(), Host, alloc());
    let l = match lower_body_expanding(text, Edition::E2024, &mut Unix, &mut crate::NoModule, &mut expander, alloc()) {
        Ok(l) => l,
        Err(need) => panic!("{text}: needs {:?}", need.atoms),
    };
    let unhandled: StdVec<_> = l.view().unhandled().collect();
    assert!(unhandled.is_empty(), "{text}: unhandled {unhandled:?}\n{}", dump_of(&l));
    assert!(l.view().expansion_keys.len() > 0, "{text}: the body's macro expanded\n{}", dump_of(&l));
    l
}

fn binds(v: &View<'_>, name: &str) -> StdVec<PatId> {
    (0..v.pats.len() as u32)
        .map(PatId)
        .filter(|p| matches!(v.pat(*p), Ok(Pat::Bind { name: n, .. }) if v.name_text(n) == Ok(name)))
        .collect()
}

/// Every local read, in the order the lowering made them.
fn locals(v: &View<'_>) -> StdVec<PatId> {
    (0..v.exprs.len() as u32)
        .filter_map(|e| match v.expr(ExprId(e)) {
            Ok(Expr::Local { pat, .. }) => Some(pat),
            _ => None,
        })
        .collect()
}

fn label_diags(text: &'static str) -> StdVec<DiagKind> {
    let l = lowered(text);
    l.view().diags().filter_map(Result::ok).map(|d| d.kind).collect()
}

/// (a) tempo's shape, and (d) the control (see the module's doc).
#[test]
fn a_template_sees_the_locals_at_its_definition() {
    let text = "fn f(block: u64) -> u64 { let log = 1; macro_rules! m { () => { log + block } } m!() }";
    let l = lowered(text);
    let v = l.view();
    let (log, block) = (binds(&v, "log"), binds(&v, "block"));
    assert_eq!(locals(&v), vec![log[0], block[0]], "the template's `log` and `block` are the parameters\n{}", dump_of(&l));
    // As tempo writes it: a parameterized template called more than once.
    let text = "fn f(log: u8, block: u8) -> u8 { macro_rules! ignored { ($n:literal) => {{ g(log, $n, block) }} } ignored!(1) + ignored!(2) }";
    let l = lowered(text);
    let v = l.view();
    let (log, block) = (binds(&v, "log"), binds(&v, "block"));
    assert_eq!(locals(&v), vec![log[0], block[0], log[0], block[0]], "each call's `log` and `block`\n{}", dump_of(&l));
    // Through a second body macro the first one calls: each template resolves at its own
    // definition.
    let text = "fn f(a: u8) -> u8 { macro_rules! inner { () => { a } } macro_rules! outer { () => { inner!() } } outer!() }";
    let l = lowered(text);
    let v = l.view();
    assert_eq!(locals(&v), binds(&v, "a"), "the nested template's `a`\n{}", dump_of(&l));
}

/// (b) Hygiene's other direction: a template does not see a binding written after its
/// definition, even one in scope at the call.
#[test]
fn a_template_does_not_see_a_later_binding() {
    let text = "fn f() -> u8 { macro_rules! m { () => { y } } let y = 1; m!() }";
    let l = lowered(text);
    let v = l.view();
    assert!(locals(&v).is_empty(), "the template's `y` is no local: the checker reports E0425\n{}", dump_of(&l));
    assert!(dump_of(&l).contains("(path y)"), "the template's `y` stays a path\n{}", dump_of(&l));
    // A later binding of the same name does not hide the one the definition saw.
    let text = "fn f() -> u8 { let y = 1; macro_rules! m { () => { y } } let y = 2; m!() }";
    let l = lowered(text);
    let v = l.view();
    let ys = binds(&v, "y");
    assert_eq!(ys.len(), 2);
    assert_eq!(locals(&v), vec![ys[0]], "the `y` in scope at the definition, not the later one\n{}", dump_of(&l));
    // A macro defined outside the body sees none of its locals (expand_tests'
    // `a_macros_locals_do_not_capture_the_callers` keeps that half).
}

/// (c) A name the caller passes resolves where the caller wrote it: after the definition is
/// fine; and the template's own `let` comes before the definition's binding.
#[test]
fn a_passed_name_resolves_at_the_call() {
    let text = "fn f() -> u8 { macro_rules! m { ($e:expr) => { $e } } let x = 1; m!(x) }";
    let l = lowered(text);
    let v = l.view();
    assert_eq!(locals(&v), binds(&v, "x"), "`$e` is the caller's `x`\n{}", dump_of(&l));
    let text = "fn f() -> u8 { macro_rules! m { ($i:ident) => { $i + 1 } } let x = 1; m!(x) }";
    let l = lowered(text);
    let v = l.view();
    assert_eq!(locals(&v), binds(&v, "x"), "`$i` is the caller's `x`\n{}", dump_of(&l));
    let text = "fn f(x: u8) -> u8 { macro_rules! m { ($e:expr) => {{ let x = 2; x + $e }} } m!(x) }";
    let l = lowered(text);
    let v = l.view();
    let xs = binds(&v, "x");
    assert_eq!(xs.len(), 2, "the parameter and the template's own");
    assert_eq!(locals(&v), vec![xs[1], xs[0]], "the template's `x` is its own let, `$e` the parameter\n{}", dump_of(&l));
}

/// Labels follow the same rule: a template's `'outer` is a loop open at the definition.
#[test]
fn a_template_sees_the_labels_at_its_definition() {
    let kinds = label_diags("fn f() { 'outer: loop { macro_rules! brk { () => { break 'outer; } } brk!(); } }");
    assert!(kinds.is_empty(), "the loop is open at the definition: {kinds:?}");
    let kinds = label_diags("fn f() { macro_rules! brk { () => { break 'outer; } } 'outer: loop { brk!(); } }");
    assert_eq!(kinds, vec![DiagKind::UndeclaredLabel], "the loop opens after the definition");
}

/// A `macro` (2.0) the body writes is a body macro too, in both its forms, its template seeing
/// the locals at its definition (def-site hygiene, the Reference's "Hygiene"). The case: std's
/// `OsString::push` (library/std/src/ffi/os_str.rs), whose `macro spec_str($T:ty) { impl
/// SpecPushTo for $T { .. } }` is written in the fn and called as `spec_str!(str);`: the
/// call's path was resolved in the module, "segment 0: not found", and the call left out
/// (`Unhandled { syntax: MACRO_CALL, why: NotExpanded }`).
///
/// Prints: `test machine::def_site::tests::a_body_macro_2_expands ... ok`. A wrong version
/// (MACRO_DEF not kept among the body's macros, or its params read as rules) leaves the
/// calls unhandled and `lowered` fails with "unhandled [..]".
#[test]
fn a_body_macro_2_expands() {
    // std's shape: items written by a `macro m(params) { body }`, called twice.
    let text = "fn f() { trait T { fn t(&self); } macro m($T:ty) { impl T for $T { fn t(&self) {} } } m!(str); m!(u8); }";
    let l = lowered(text);
    assert_eq!(l.view().expansion_keys.len(), 2, "both calls expanded\n{}", dump_of(&l));
    // The parameter form's template sees the local at its definition.
    let text = "fn f(a: u8) -> u8 { macro m($e:expr) { $e + a } m!(1) }";
    let l = lowered(text);
    let v = l.view();
    assert_eq!(locals(&v), binds(&v, "a"), "the template's `a` is the parameter\n{}", dump_of(&l));
    // The arms form.
    let text = "fn f(a: u8) -> u8 { macro twice { ($x:expr) => { $x * 2 } } twice!(a) }";
    let l = lowered(text);
    let v = l.view();
    assert_eq!(locals(&v), binds(&v, "a"), "`$x` is the caller's `a`\n{}", dump_of(&l));
}

/// A template's own `let` of a name the definition also sees is the one its uses find: its
/// own hygiene's key is asked before the definition's.
#[test]
fn a_shadowing_template_let_is_its_own() {
    let text = "fn f(log: u8) -> u8 { macro_rules! m { () => {{ let log = 2; log }} } m!() }";
    let l = lowered(text);
    let v = l.view();
    let logs = binds(&v, "log");
    assert_eq!(logs.len(), 2);
    assert_eq!(locals(&v), vec![logs[1]], "the template's own `let log` comes first\n{}", dump_of(&l));
}
