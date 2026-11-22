//! The worlds the rule tests check, each with the keys its test looks at. The determinism
//! property runs all of them.

use super::{V, World};
use crate::{DefKind, Edition, ExternTarget, CallShape, Namespace, Rename};
use blaze_id::ItemKey;
use std::vec::Vec;

pub(crate) struct Named {
    pub(crate) root: ItemKey,
    pub(crate) foo: ItemKey,
}

/// `mod a { pub fn foo() {} } use a::foo; use a::foo as bar;`
pub(crate) fn named() -> (World, Named) {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let a = w.module(root, "a", V::Priv);
    let foo = w.fun(a, "foo", V::Pub);
    w.use_(root, "a::foo", V::Priv);
    w.use_as(root, "a::foo", "bar", V::Priv);
    (w, Named { root, foo })
}

pub(crate) struct Shadow {
    pub(crate) root: ItemKey,
    pub(crate) a_bar: ItemKey,
    pub(crate) b_foo: ItemKey,
}

/// `mod a { pub fn foo; pub fn bar; } mod b { pub fn foo; } use a::*; use b::foo;`
pub(crate) fn shadow() -> (World, Shadow) {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let a = w.module(root, "a", V::Priv);
    w.fun(a, "foo", V::Pub);
    let a_bar = w.fun(a, "bar", V::Pub);
    let b = w.module(root, "b", V::Priv);
    let b_foo = w.fun(b, "foo", V::Pub);
    w.glob(root, "a", V::Priv);
    w.use_(root, "b::foo", V::Priv);
    (w, Shadow { root, a_bar, b_foo })
}

pub(crate) struct TwoGlobs {
    pub(crate) root: ItemKey,
    pub(crate) user: ItemKey,
}

/// `mod a { pub fn x; } mod b { pub fn x; } use a::*; use b::*; mod user { use super::x; }`
pub(crate) fn two_globs() -> (World, TwoGlobs) {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let a = w.module(root, "a", V::Priv);
    w.fun(a, "x", V::Pub);
    let b = w.module(root, "b", V::Priv);
    w.fun(b, "x", V::Pub);
    w.glob(root, "a", V::Priv);
    w.glob(root, "b", V::Priv);
    let user = w.module(root, "user", V::Priv);
    w.use_(user, "super::x", V::Priv);
    (w, TwoGlobs { root, user })
}

pub(crate) struct Chain {
    pub(crate) root: ItemKey,
    pub(crate) c: ItemKey,
    pub(crate) f: ItemKey,
}

/// `mod a { pub fn f; } mod b { pub use crate::a::f; } mod c { pub use crate::b::f; } use c::f;`
pub(crate) fn chain() -> (World, Chain) {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let a = w.module(root, "a", V::Priv);
    let f = w.fun(a, "f", V::Pub);
    let b = w.module(root, "b", V::Priv);
    w.use_(b, "crate::a::f", V::Pub);
    let c = w.module(root, "c", V::Priv);
    w.use_(c, "crate::b::f", V::Pub);
    w.use_(root, "c::f", V::Priv);
    (w, Chain { root, c, f })
}

pub(crate) struct SuperGlob {
    pub(crate) root: ItemKey,
    pub(crate) child: ItemKey,
    pub(crate) r: ItemKey,
    pub(crate) c: ItemKey,
}

/// `mod child { use super::*; pub fn c; } pub use child::*; pub fn r;`
pub(crate) fn super_glob() -> (World, SuperGlob) {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let child = w.module(root, "child", V::Priv);
    w.glob(child, "super", V::Priv);
    let c = w.fun(child, "c", V::Pub);
    w.glob(root, "child", V::Pub);
    let r = w.fun(root, "r", V::Pub);
    (w, SuperGlob { root, child, r, c })
}

pub(crate) struct Private {
    pub(crate) root: ItemKey,
    pub(crate) a: ItemKey,
    pub(crate) inner: ItemKey,
    pub(crate) hidden: ItemKey,
    pub(crate) shown: ItemKey,
}

/// `mod a { fn hidden; pub fn shown; mod inner { use super::*; } } use a::*;`
pub(crate) fn private() -> (World, Private) {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let a = w.module(root, "a", V::Priv);
    let hidden = w.fun(a, "hidden", V::Priv);
    let shown = w.fun(a, "shown", V::Pub);
    let inner = w.module(a, "inner", V::Priv);
    w.glob(inner, "super", V::Priv);
    w.glob(root, "a", V::Priv);
    (w, Private { root, a, inner, hidden, shown })
}

pub(crate) struct Restricted {
    pub(crate) a: ItemKey,
    pub(crate) b: ItemKey,
    pub(crate) c: ItemKey,
}

/// `mod a { pub mod b { pub(crate) fn pc; pub(in crate::a) fn pa; pub(super) fn ps;
/// pub(in crate::c) fn bad; } } mod c { use crate::a::b::pc; use crate::a::b::pa; }`
pub(crate) fn restricted() -> (World, Restricted) {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let a = w.module(root, "a", V::Priv);
    let b = w.module(a, "b", V::Pub);
    w.fun(b, "pc", V::Crate);
    w.fun(b, "pa", V::In("crate::a"));
    w.fun(b, "ps", V::Super);
    w.fun(b, "bad", V::In("crate::c"));
    let c = w.module(root, "c", V::Priv);
    w.use_(c, "crate::a::b::pc", V::Priv);
    w.use_(c, "crate::a::b::pa", V::Priv);
    (w, Restricted { a, b, c })
}

pub(crate) struct Extern {
    pub(crate) root: ItemKey,
    pub(crate) a: ItemKey,
    pub(crate) dep_root: ItemKey,
    pub(crate) d: ItemKey,
}

/// Crate `dep` with `pub fn d`; the main crate: `use dep::d; mod a { extern crate dep as
/// renamed; extern crate nothere; }`.
pub(crate) fn extern_crates() -> (World, Extern) {
    let mut w = World::new();
    let dep_root = w.root(2, Edition::E2021);
    let d = w.fun(dep_root, "d", V::Pub);
    let root = w.root(1, Edition::E2021);
    w.dep(root, "dep", dep_root);
    w.use_(root, "dep::d", V::Priv);
    let a = w.module(root, "a", V::Priv);
    w.extern_crate(a, ExternTarget::Dep("dep"), Rename::To("renamed"), V::Priv);
    w.extern_crate(a, ExternTarget::Dep("nothere"), Rename::Same, V::Priv);
    (w, Extern { root, a, dep_root, d })
}

pub(crate) struct Prelude {
    pub(crate) a: ItemKey,
    pub(crate) b: ItemKey,
    pub(crate) vec_struct: ItemKey,
}

/// A `std` with `pub mod vec { pub struct Vec; } pub mod prelude { pub use crate::vec; }` as
/// the main crate's prelude; `mod a { use vec::Vec; } #[no_implicit_prelude] mod b { use
/// vec::Vec; }`.
pub(crate) fn prelude() -> (World, Prelude) {
    let mut w = World::new();
    let std_root = w.root(3, Edition::E2021);
    let vec_mod = w.module(std_root, "vec", V::Pub);
    let vec_struct = w.item(vec_mod, "Vec", Namespace::Types, DefKind::Other, V::Pub);
    let p = w.module(std_root, "prelude", V::Pub);
    w.use_(p, "crate::vec", V::Pub);
    let root = w.root(1, Edition::E2021);
    w.cr(root).head.prelude = Some(p);
    let a = w.module(root, "a", V::Priv);
    w.use_(a, "vec::Vec", V::Priv);
    let b = w.module_with(root, "b", V::Priv, false, true);
    w.use_(b, "vec::Vec", V::Priv);
    (w, Prelude { a, b, vec_struct })
}

pub(crate) struct Textual {
    pub(crate) root: ItemKey,
    pub(crate) before_call: ItemKey,
    pub(crate) after_call: ItemKey,
}

/// `m!(); macro_rules! m {} m!();`
pub(crate) fn textual() -> (World, Textual) {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let before_call = w.call(root, "m", CallShape::FnLike);
    w.rules(root, "m");
    let after_call = w.call(root, "m", CallShape::FnLike);
    (w, Textual { root, before_call, after_call })
}

pub(crate) struct MacroUse {
    pub(crate) root: ItemKey,
    pub(crate) before: ItemKey,
    pub(crate) after: ItemKey,
    pub(crate) m: ItemKey,
}

/// `mod before { m!(); } #[macro_use] mod a { macro_rules! m {} } m!(); mod after { m!(); }`
pub(crate) fn macro_use() -> (World, MacroUse) {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let before = w.module(root, "before", V::Priv);
    w.call(before, "m", CallShape::FnLike);
    let a = w.module_with(root, "a", V::Priv, true, false);
    let m = w.rules(a, "m");
    w.call(root, "m", CallShape::FnLike);
    let after = w.module(root, "after", V::Priv);
    w.call(after, "m", CallShape::FnLike);
    (w, MacroUse { root, before, after, m })
}

pub(crate) struct Export {
    pub(crate) root: ItemKey,
    pub(crate) dep_root: ItemKey,
    pub(crate) exp: ItemKey,
}

/// Crate `dep`: `mod inner { #[macro_export] macro_rules! exp {} }`; the main crate:
/// `dep::exp!();`.
pub(crate) fn macro_export() -> (World, Export) {
    let mut w = World::new();
    let dep_root = w.root(2, Edition::E2021);
    let inner = w.module(dep_root, "inner", V::Priv);
    let exp = w.rules(inner, "exp");
    w.cr(dep_root).macro_exports.push(("exp", exp, crate::MacroShapes::FN_LIKE));
    let root = w.root(1, Edition::E2021);
    w.dep(root, "dep", dep_root);
    w.call(root, "dep::exp", CallShape::FnLike);
    (w, Export { root, dep_root, exp })
}

pub(crate) struct Expanded {
    pub(crate) root: ItemKey,
    pub(crate) user: ItemKey,
    pub(crate) made: ItemKey,
}

/// `macro_rules! gen {} gen!(); use self::made as alias; mod user { use super::made; }`,
/// where `gen!()` expands to `pub fn made() {}`.
pub(crate) fn expanded() -> (World, Expanded) {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    w.rules(root, "gen");
    let call = w.call(root, "gen", CallShape::FnLike);
    let made = w.fun(call, "made", V::Pub);
    w.use_as(root, "self::made", "alias", V::Priv);
    let user = w.module(root, "user", V::Priv);
    w.use_(user, "super::made", V::Priv);
    (w, Expanded { root, user, made })
}

pub(crate) struct GlobExpanded {
    pub(crate) root: ItemKey,
    pub(crate) tools_expanded: ItemKey,
}

/// `mod a { pub mod tools { pub fn t; } } use a::*; macro_rules! gen {} gen!(); use
/// tools::t;`, where `gen!()` expands to `mod tools { pub fn t; }`.
pub(crate) fn glob_vs_expanded() -> (World, GlobExpanded) {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let a = w.module(root, "a", V::Priv);
    let tools = w.module(a, "tools", V::Pub);
    w.fun(tools, "t", V::Pub);
    w.glob(root, "a", V::Priv);
    w.rules(root, "gen");
    let call = w.call(root, "gen", CallShape::FnLike);
    let tools_expanded = w.module(call, "tools", V::Priv);
    w.fun(tools_expanded, "t", V::Pub);
    w.use_(root, "tools::t", V::Priv);
    (w, GlobExpanded { root, tools_expanded })
}

pub(crate) struct Negative {
    pub(crate) root: ItemKey,
    pub(crate) dep_root: ItemKey,
    pub(crate) prelude: ItemKey,
}

/// A `#[macro_use]` crate `dep` with no `pm`, and a prelude with a bang macro `pm`; the main
/// crate: `pm!();`.
pub(crate) fn negative() -> (World, Negative) {
    let mut w = World::new();
    let dep_root = w.root(2, Edition::E2021);
    let std_root = w.root(3, Edition::E2021);
    let prelude = w.module(std_root, "prelude", V::Pub);
    w.item(prelude, "pm", Namespace::Macros, DefKind::Macro(crate::MacroShapes::FN_LIKE), V::Pub);
    let root = w.root(1, Edition::E2021);
    w.cr(root).macro_use.push(dep_root);
    w.cr(root).head.prelude = Some(prelude);
    w.call(root, "pm", CallShape::FnLike);
    (w, Negative { root, dep_root, prelude })
}

pub(crate) struct FailedImport {
    pub(crate) a: ItemKey,
    pub(crate) a_child: ItemKey,
}

/// The edit property's least failing case (tape `1`):
/// `mod a { pub fn x; mod a_child { use super::fresh as got; } }`, and with `fresh`, a private
/// `fn fresh` in `a`. Before it, the import fails; its negative reads of `a` are what tell
/// `a_child` that the edit concerns it.
pub(crate) fn failed_import(fresh: bool) -> (World, FailedImport) {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let a = w.module(root, "a", V::Priv);
    w.fun(a, "x", V::Pub);
    let a_child = w.module(a, "a_child", V::Priv);
    w.use_as(a_child, "super::fresh", "got", V::Priv);
    if fresh {
        w.fun(a, "fresh", V::Priv);
    }
    (w, FailedImport { a, a_child })
}

/// Every fixture's world: what the determinism property permutes.
pub(crate) fn all() -> Vec<World> {
    std::vec![
        named().0,
        shadow().0,
        two_globs().0,
        chain().0,
        super_glob().0,
        private().0,
        restricted().0,
        extern_crates().0,
        prelude().0,
        textual().0,
        macro_use().0,
        macro_export().0,
        expanded().0,
        glob_vs_expanded().0,
        negative().0,
        failed_import(false).0,
    ]
}
