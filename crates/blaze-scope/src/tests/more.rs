//! Tests of the rules added after the first round: the group's glob fixpoint, an
//! `Undetermined` answer reaching the final pass, derive helpers, built-in attributes,
//! `use .. as _`, macro-expanded `#[macro_export]`, and `pub(in path)` through an alias.

use std::vec::Vec;

use blaze_id::ItemKey;
use super::{N, Solved, V, World, solve};
use crate::{
    AmbiguityKind, DefKind, Edition, CallShape, Namespace, ScopeError, Unresolved, Visibility,
};

fn solved(w: &World) -> Solved {
    match solve(w, None) {
        Ok(s) => s,
        Err(e) => panic!("the driver failed: {e}"),
    }
}

fn macro_error(s: &Solved, m: ItemKey, call: ItemKey) -> Option<Unresolved> {
    s.errors(m).iter().find_map(|e| match e {
        ScopeError::Macro { call: c, why, .. } if *c == call => Some(*why),
        _ => None,
    })
}

const MODS: [N; 12] = ["m0", "m1", "m2", "m3", "m4", "m5", "m6", "m7", "m8", "m9", "m10", "m11"];
const PATHS: [&str; 12] = [
    "crate::m0", "crate::m1", "crate::m2", "crate::m3", "crate::m4", "crate::m5", "crate::m6",
    "crate::m7", "crate::m8", "crate::m9", "crate::m10", "crate::m11",
];
const FUNS: [N; 12] = ["f0", "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11"];

/// Rule: a cycle of globs binds what is bound somewhere on it. Twelve modules in a ring, each
/// `pub use` of both neighbours' globs, each see every module's function: the group's least
/// fixpoint, reached in time polynomial in the ring (a search of the ring's paths is not).
#[test]
fn a_ring_of_globs_reaches_its_fixpoint() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let mut ms = Vec::new();
    let mut fs = Vec::new();
    for i in 0..12 {
        ms.push(w.module(root, MODS[i], V::Pub));
    }
    for i in 0..12 {
        fs.push(w.fun(ms[i], FUNS[i], V::Pub));
        w.glob(ms[i], PATHS[(i + 1) % 12], V::Pub);
        w.glob(ms[i], PATHS[(i + 11) % 12], V::Pub);
    }
    let s = solved(&w);
    for i in 0..12 {
        for j in 0..12 {
            assert_eq!(s.def(ms[i], Namespace::Values, FUNS[j]), Some(fs[j]), "m{i} sees f{j}");
        }
        assert!(s.errors(ms[i]).is_empty(), "{:?}", s.errors(ms[i]));
    }
}

/// Rule: a derive's helper attributes are in scope for the attributes after it on the same
/// item, and only there (the Reference, "Derive macro helper attributes"). Expected: "test
/// blaze-scope: exit 0; 1 passed, 0 failed"; without names-attr-3's helper scope
/// (`Cells::derive_helper`) `after` fails NotFound.
#[test]
fn derive_helpers_scope_the_attributes_after_them() {
    let mut w = World::new();
    let dep_root = w.root(2, Edition::E2021);
    let _ser = w.derive_macro(dep_root, "Ser", &["serde"]);
    let root = w.root(1, Edition::E2021);
    w.dep(root, "dep", dep_root);
    w.use_(root, "dep::Ser", V::Priv);
    w.call_on(root, "Ser", CallShape::Derive, Some(1));
    let after = w.call_on(root, "serde", CallShape::Attr, Some(1));
    let other_item = w.call_on(root, "serde", CallShape::Attr, Some(2));
    let before = w.call_on(root, "serde", CallShape::Attr, Some(3));
    w.call_on(root, "Ser", CallShape::Derive, Some(3));
    let s = solved(&w);
    assert_eq!(macro_error(&s, root, after), None, "{:?}", s.errors(root));
    assert_eq!(macro_error(&s, root, other_item), Some(Unresolved::NotFound));
    assert_eq!(macro_error(&s, root, before), Some(Unresolved::NotFound));
    
}

/// Rule: a built-in attribute is inert, and an attribute macro of a nearer scope with its
/// name is ambiguous with it (Reference "Built-in attributes index"; "Names" / name
/// resolution: an ambiguous name is error E0659; rustc's built-in attribute scope comes after
/// the module's own and the `#[macro_use]` prelude's names). `#[inline]` at the root with no
/// macro of the name: no error; `#[inline]` in `m` beside `use super::inline;` of an attribute
/// macro: `Ambiguous(BuiltinAttr)`. `test` is no built-in attribute (the pinned library:
/// `#[rustc_builtin_macro] pub macro test`, a prelude macro), so `#[test]` in `m` beside
/// `use super::test;` names that macro with no ambiguity (names-attr-3). Expected: "test
/// blaze-scope: exit 0; 1 passed, 0 failed"; without names-G2-G3's built-in attribute scope
/// `#[inline]` fails NotFound; with `test` in the root's `builtin_attrs` `#[test]` is ambiguous.
#[test]
fn builtin_attributes_are_inert_and_shadowing_them_is_ambiguous() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let m = w.module(root, "m", V::Priv);
    // `n` binds no `inline`: a module's scope holds its own items, not its parent's.
    let n = w.module(root, "n", V::Priv);
    let inline = w.call_on(n, "inline", CallShape::Attr, Some(1));
    w.item(root, "inline", Namespace::Macros, DefKind::Macro(crate::MacroShapes::ATTR), V::Priv);
    w.item(root, "test", Namespace::Macros, DefKind::Macro(crate::MacroShapes::ATTR), V::Priv);
    let shadowed = w.call_on(m, "inline", CallShape::Attr, Some(1));
    let test = w.call_on(m, "test", CallShape::Attr, Some(2));
    w.use_(m, "super::inline", V::Priv);
    w.use_(m, "super::test", V::Priv);
    let s = solved(&w);
    assert_eq!(macro_error(&s, m, shadowed), Some(Unresolved::Ambiguous(AmbiguityKind::BuiltinAttr)));
    assert_eq!(macro_error(&s, m, test), None, "`test` is a prelude macro: {:?}", s.errors(m));
    assert_eq!(macro_error(&s, n, inline), None, "{:?}", s.errors(n));
}

/// Rule: `use Trait as _;` binds no name and brings the trait into scope.
#[test]
fn underscore_import_brings_a_trait_into_scope() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let a = w.module(root, "a", V::Priv);
    let t = w.item(a, "T", Namespace::Types, DefKind::Trait, V::Pub);
    w.use_anon(root, "a::T");
    let s = solved(&w);
    assert_eq!(s.def(root, Namespace::Types, "T"), None);
    let traits = &s.scope(root).traits;
    assert_eq!(traits.len(), 1);
    assert_eq!(traits[0].def.key, t);
    assert!(s.errors(root).is_empty(), "{:?}", s.errors(root));
}

/// Rule: a `#[macro_export]` macro an expansion writes is exported from the crate, listed by
/// the module it was written in for the crate's records.
#[test]
fn a_macro_expanded_macro_export_is_listed() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let m = w.module(root, "m", V::Priv);
    w.rules(m, "gen");
    let call = w.call(m, "gen", CallShape::FnLike);
    let exp = w.rules_with(call, "exp", true);
    let s = solved(&w);
    let exports = &s.scope(m).macro_exports;
    assert_eq!(exports.len(), 1);
    assert_eq!((exports[0].name, exports[0].def, exports[0].expansion), ("exp", exp, 1));
}

/// Rule: a crate's prelude may be its own module (`core`: `#![no_core]` and `#[prelude_import]
/// use prelude::rust_2024::*`), in one cycle with the modules that read it. The lexical
/// prelude layer then reads a member of the group: it is read live, never asked for. `mod p {
/// pub use crate::A; pub use crate::m::B; } mod m { pub struct B; } pub struct A; use B as
/// C;` with `p` the prelude: `C` is `m::B` (core's 192-module group stalled on this read).
#[test]
fn a_prelude_in_the_crate_s_own_cycle_is_read_live() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let p = w.module(root, "p", V::Pub);
    let m = w.module(root, "m", V::Pub);
    let b = w.item(m, "B", Namespace::Types, DefKind::Other, V::Pub);
    w.item(root, "A", Namespace::Types, DefKind::Other, V::Pub);
    w.use_(p, "crate::A", V::Pub);
    w.use_(p, "crate::m::B", V::Pub);
    w.cr(root).head.prelude = Some(p);
    w.use_as(root, "B", "C", V::Pub);
    let s = solved(&w);
    assert!(s.errors(root).is_empty(), "{:?}", s.errors(root));
    assert_eq!(s.def(root, Namespace::Types, "C"), Some(b));
}

/// Rule: a macro path through a named import of the crate root resolves once the import does.
/// core's shape: `#[macro_use] mod macros { pub macro cfg_select }`, `pub use
/// crate::macros::cfg_select;`, a prelude module of the crate globbing the root, and `mod ffi {
/// mod va_list { crate::cfg_select!(); } }`.
#[test]
fn a_macro_path_through_a_root_import() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2024);
    let macros = w.module_with(root, "macros", V::Priv, true, false);
    let mac = w.item(macros, "cfg_select", Namespace::Macros, DefKind::Macro(crate::MacroShapes::FN_LIKE), V::Pub);
    let p = w.module(root, "prelude", V::Pub);
    w.glob(p, "crate::macros", V::Pub);
    w.cr(root).head.prelude = Some(p);
    w.use_(root, "crate::macros::cfg_select", V::Pub);
    let ffi = w.module(root, "ffi", V::Pub);
    let va = w.module(ffi, "va_list", V::Priv);
    let call = w.call(va, "crate::cfg_select", CallShape::FnLike);
    w.fun(call, "made", V::Pub);
    let s = solved(&w);
    assert_eq!(macro_error(&s, va, call), None, "{:?}", s.errors(va));
    assert_eq!(s.def(root, Namespace::Macros, "cfg_select"), Some(mac));
    assert!(s.def(va, Namespace::Values, "made").is_some());
}

/// Rule: an import's macro reads the `macro_rules!` scope at its place. `mod u { macro_rules!
/// m {} pub(crate) use m; } use u::m as n;` (core's `ub_checks::assert_unsafe_precondition`):
/// `u` binds `m` to the macro, and the root's `n` is it.
#[test]
fn a_macro_rules_is_imported_by_name() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let u = w.module(root, "u", V::Pub);
    let m = w.rules(u, "m");
    w.use_(u, "m", V::Crate);
    w.use_as(root, "u::m", "n", V::Priv);
    let s = solved(&w);
    assert!(s.errors(u).is_empty(), "{:?}", s.errors(u));
    assert!(s.errors(root).is_empty(), "{:?}", s.errors(root));
    assert_eq!(s.def(u, Namespace::Macros, "m"), Some(m));
    assert_eq!(s.def(root, Namespace::Macros, "n"), Some(m));

    // `#[macro_export] macro_rules! e {} pub use e;` is public; without the export, a wider
    // re-export.
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let u = w.module(root, "u", V::Pub);
    w.rules_with(u, "e", true);
    w.use_(u, "e", V::Pub);
    w.rules(u, "p");
    w.use_(u, "p", V::Pub);
    let s = solved(&w);
    let wider: Vec<_> = s
        .errors(u)
        .iter()
        .filter_map(|e| match e {
            ScopeError::WiderReexport { name, .. } => Some(*name),
            _ => None,
        })
        .collect();
    assert_eq!(wider, ["p"], "{:?}", s.errors(u));
}

/// Rule: the builtin types are the last layer of the type namespace, `#[no_implicit_prelude]`
/// or not, and a nearer binding shadows one. `pub use bool;` (core's `primitive.rs`) binds the
/// builtin; with `mod u8 {}` beside it, `use u8;` is the module.
#[test]
fn builtin_types_are_the_last_layer() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let p = w.module_with(root, "p", V::Pub, false, true);
    w.use_(p, "bool", V::Pub);
    let u8_mod = w.module(root, "u8", V::Pub);
    w.use_as(root, "u8", "byte", V::Priv);
    let s = solved(&w);
    assert!(s.errors(p).is_empty(), "{:?}", s.errors(p));
    let bool_def = crate::builtin_type("bool").map(|d| d.key);
    assert!(bool_def.is_some_and(crate::is_builtin_type));
    assert_eq!(s.def(p, Namespace::Types, "bool"), bool_def);
    assert_eq!(s.def(root, Namespace::Types, "byte"), Some(u8_mod));
}

/// Rule: `pub(in path)` names a module by path, an alias of an ancestor included (Reference
/// `vis.scoped.in`: "`path` must be a simple path which resolves to an ancestor module of the
/// item"); a path to a module that is no ancestor is an error at that item, which is then
/// private to its module (`g`: `Restricted(b)`), and the crate still resolves.
#[test]
fn pub_in_path_through_an_alias() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let a = w.module(root, "a", V::Pub);
    let b = w.module(a, "b", V::Pub);
    w.module(root, "c", V::Pub);
    w.use_as(root, "a", "x", V::Pub);
    w.use_as(root, "c", "y", V::Pub);
    w.fun(b, "f", V::In("crate::x"));
    w.fun(b, "g", V::In("crate::y"));
    let s = solved(&w);
    let vis = |name: N| s.scope(b).get(Namespace::Values, name).map(|b| b.vis);
    assert_eq!(vis("f"), Some(Visibility::Restricted(a)));
    assert_eq!(vis("g"), Some(Visibility::Restricted(b)));
    let bad: Vec<_> =
        s.errors(b).iter().filter(|e| matches!(e, ScopeError::RestrictedNotAncestor { .. })).collect();
    assert_eq!(bad.len(), 1, "{:?}", s.errors(b));
    
}
