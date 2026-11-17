//! names-G2-G3-2: the lowering sends a kept item's built-in and tool attributes to the cells,
//! which answer them, on the real path (`resolve::resolve_crates`, the task the command runs).

use std::string::String;
use std::vec::Vec;

use blaze_id::ItemKey;

use super::entry::{Texts, input, resolved};
use crate::{AmbiguityKind, CallShape, ScopeError, Unresolved};

/// A `core` of two macros: the builtin `Clone` derive, and a `macro_rules!` attribute macro
/// that keeps its item and writes `wrapped` beside it.
const DEP: &str = "#![no_core]
#[rustc_builtin_macro]
pub macro Clone($item:item) {}
#[macro_export]
macro_rules! wrap {
    attr() ($v:vis fn $name:ident() {}) => {
        $v fn $name() {}
        pub fn wrapped() {}
    };
}
";

const USER: &str = "#![no_core]
use dep::Clone;
#[inline]
pub fn kept() {}
#[derive(Clone)]
pub struct Derived;
#[rustfmt::skip]
pub fn skipped() {}
#[dep::wrap]
pub fn input() {}
macro_rules! inline {
    attr() ($i:item) => { $i };
}
#[inline]
pub fn shadowed() {}
";

static TEXTS: Texts = Texts(&[&[("/user/lib.rs", USER)], &[("/dep/lib.rs", DEP)]]);

/// Rule: a built-in attribute is inert, a tool attribute is inert, an attribute macro takes its
/// item, and an attribute macro a nearer scope binds under a built-in attribute's name is
/// ambiguous with the built-in (the Reference, "Attributes" `attributes.builtin` and
/// `attributes.tool`: "the tool attributes .. `rustfmt` and `clippy`"; "Names" / name
/// resolution: an ambiguous name is error E0659; rustc's built-in attribute scope follows the
/// textual scope, so `macro_rules! inline` written before `#[inline]` is ambiguous with it).
///
/// Through the real path: the first `#[inline]` and `#[rustfmt::skip]` reach the cells as
/// attribute calls and fail nothing (`Outcome::Inert`); `#[derive(Clone)]` is the derive
/// (`derive` is the prelude's attribute macro, read by the lowering, not sent; a helper after it
/// is names-attr-4's, tests/names_attr_4.rs); `#[dep::wrap]` is called (`wrapped` exists); the
/// `#[inline]` after `macro_rules! inline` is `Ambiguous(BuiltinAttr)` on that call, the only
/// macro error. The two `#[inline]`s are two calls: the textual scope grew between them.
///
/// Expected: "test blaze-scope: exit 0; 1 passed, 0 failed". Control: without names-G2-G3-2's
/// `Lower::attr_checks` no `inline` call is lowered ("built-in attributes reach the cells")
/// and the ambiguity is not reported.
#[test]
fn builtin_attributes_reach_the_cells_on_the_real_path() {
    let crates = [input("user", "/user/lib.rs", &[("dep", 1)]), input("dep", "/dep/lib.rs", &[])];
    let names = resolved(&crates, &TEXTS);
    let user = names.krate(0);
    let root = names.root(0);
    let decls = &user.decls.get(&root).expect("the root is lowered").1;
    let attr_calls: Vec<(String, ItemKey, u32)> =
        decls.calls.iter().filter(|c| c.kind == CallShape::Attr).map(|c| (c.path.segments.join("::"), c.call, c.pos)).collect();
    let named = |path: &str| attr_calls.iter().filter(|(p, ..)| p == path).map(|&(_, call, pos)| (call, pos)).collect::<Vec<_>>();
    let mut inline = named("inline");
    inline.sort_by_key(|&(_, pos)| pos);
    assert_eq!(inline.len(), 2, "built-in attributes reach the cells, one per textual span: {attr_calls:?}");
    assert_eq!(named("rustfmt::skip").len(), 1, "a tool attribute reaches the cells: {attr_calls:?}");
    assert_eq!(named("dep::wrap").len(), 1, "the attribute macro is a call: {attr_calls:?}");
    assert!(named("derive").is_empty(), "the derive is the lowering's: {attr_calls:?}");

    let errors = &user.done.get(&root).expect("the root is done").scope.errors;
    let macro_errors: Vec<(ItemKey, String, Unresolved)> = errors
        .iter()
        .filter_map(|e| match e {
            ScopeError::Macro { call, path, why, .. } => Some((*call, path.segments.join("::"), *why)),
            _ => None,
        })
        .collect();
    let shadowed = inline[1].0;
    assert_eq!(
        macro_errors,
        [(shadowed, String::from("inline"), Unresolved::Ambiguous(AmbiguityKind::BuiltinAttr))],
        "only the `#[inline]` after `macro_rules! inline` fails, ambiguous; errors {errors:?}; notes {:?}",
        names.notes(0)
    );

    let texts: Vec<Option<&str>> = user.tables.texts.values().map(|(t, _)| t.name).collect();
    for name in ["kept", "Derived", "skipped", "input", "wrapped", "shadowed"] {
        assert!(texts.contains(&Some(name)), "`{name}` is declared with its text: {texts:?}");
    }
    let headers: Vec<&str> = user.tables.impls.iter().map(|i| i.header).collect();
    assert!(headers.iter().any(|h| h.contains("Clone") && h.contains("Derived")), "the derive ran: {headers:?}");
}
