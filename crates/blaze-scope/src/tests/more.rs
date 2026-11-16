//! Tests of the rules added after the first round: the group's glob fixpoint, an
//! `Undetermined` answer reaching the final pass, derive helpers, built-in attributes,
//! `use .. as _`, macro-expanded `#[macro_export]`, and `pub(in path)` through an alias.

use std::alloc::Global;
use std::vec::Vec;

use blaze_id::ItemKey;

use super::{N, Solved, V, World, solve};
use crate::resolver::Plant;
use crate::{
    AmbiguityKind, BindingState, DefKind, Edition, ExportEntry, CallShape, Namespace, Read, Reply,
    Resolver, ScopeError, Step, Unresolved, Visibility,
};

fn solved(w: &World) -> Solved {
    match solve(w, None, Plant::None) {
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
        assert_eq!(s.scope(ms[i]).fellows.len(), 11);
        assert!(s.errors(ms[i]).is_empty(), "{:?}", s.errors(ms[i]));
    }
}

/// Rule: a name that only an `Undetermined` answer could bind, once nothing can move, is an
/// unresolved import of the glob that brought it, a value in the scope.
#[test]
fn an_undetermined_answer_is_an_unresolved_import() {
    let mut w = World::new();
    let dep_root = w.root(2, Edition::E2021);
    let u = w.fun(dep_root, "u", V::Pub);
    let root = w.root(1, Edition::E2021);
    w.dep(root, "dep", dep_root);
    w.glob(root, "dep", V::Priv);
    let mut r = Resolver::new_in(root, &w, Global).expect("the root's records are there");
    let undetermined = [ExportEntry {
        ns: Namespace::Values,
        name: "u",
        def: crate::Def { key: u, kind: DefKind::Other },
        vis: Visibility::Public,
        state: BindingState::Undetermined,
    }];
    let digest = blaze_id::ContentId([9; 32]);
    let mut asked: Vec<Read<N>> = Vec::new();
    for _ in 0..16 {
        let replies: Vec<Reply<'_, N>> = asked
            .iter()
            .map(|&read| Reply { read, answer: crate::Answer::Exports { entries: &undetermined, digest } })
            .collect();
        match r.step(&w, &replies).expect("the machine steps") {
            Step::Ask(reads) => {
                assert!(reads.iter().all(|r| matches!(r, Read::Exports { module, .. } if *module == dep_root)));
                asked = reads.to_vec();
            }
            Step::Done(done) => {
                assert_eq!(done.scope.get(Namespace::Values, "u"), None);
                assert!(
                    done.scope.errors.iter().any(|e| matches!(e, ScopeError::Import { why: Unresolved::Undetermined, .. })),
                    "{:?}",
                    done.scope.errors
                );
                return;
            }
            Step::Expand(_) | Step::Cycle(_) => panic!("the machine needs only exports"),
        }
    }
    panic!("the machine did not finish in 16 steps");
}

/// Rule: a derive's helper attributes are in scope for the attributes after it on the same
/// item, and only there; the helper list is a read of the read list.
#[test]
fn derive_helpers_scope_the_attributes_after_them() {
    let mut w = World::new();
    let dep_root = w.root(2, Edition::E2021);
    let ser = w.derive_macro(dep_root, "Ser", &["serde"]);
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
    assert!(s.by[&root].reads.contains(&Read::DeriveHelpers { derive: ser }));
}

/// Rule: a built-in attribute is inert, and an attribute macro of a nearer scope with its
/// name is ambiguous with it.
#[test]
fn builtin_attributes_are_inert_and_shadowing_them_is_ambiguous() {
    let mut w = World::new();
    let root = w.root(1, Edition::E2021);
    let inline = w.call_on(root, "inline", CallShape::Attr, Some(1));
    let m = w.module(root, "m", V::Priv);
    w.item(root, "test", Namespace::Macros, DefKind::Macro(CallShape::Attr), V::Priv);
    let test = w.call_on(m, "test", CallShape::Attr, Some(1));
    w.use_(m, "super::test", V::Priv);
    let s = solved(&w);
    assert_eq!(macro_error(&s, root, inline), None, "{:?}", s.errors(root));
    assert_eq!(macro_error(&s, m, test), Some(Unresolved::Ambiguous(AmbiguityKind::BuiltinAttr)));
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

/// Rule: `pub(in path)` names a module by path, an alias of an ancestor included; a path to a
/// module that is no ancestor is an error.
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
    assert!(s.by[&b].reads.contains(&Read::Name { module: root, ns: Namespace::Types, name: "x" }));
}
