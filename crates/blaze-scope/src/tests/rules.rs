//! One test per rule of Rust's name resolution the machine follows; each says the rule.

use super::fixtures;
use super::{Solved, solve};
use crate::resolver::Plant;
use crate::{
    AmbiguityKind, BindingState, Def, NameAnswer, Namespace, Origin, Read, Saw, ScopeError, Site,
    Unresolved, Visibility,
};

use super::{N, World, crate_key};

fn solved(w: &World) -> Solved {
    match solve(w, None, Plant::None) {
        Ok(s) => s,
        Err(e) => panic!("the driver failed: {e}"),
    }
}

const T: Namespace = Namespace::Types;
const VAL: Namespace = Namespace::Values;
const MAC: Namespace = Namespace::Macros;

fn import_error(errors: &[ScopeError<N, std::alloc::Global>]) -> Option<(u32, Unresolved)> {
    errors.iter().find_map(|e| match e {
        ScopeError::Import { segment, why, .. } => Some((*segment, *why)),
        _ => None,
    })
}

/// Rule: `use a::b;` binds the target under its last segment, and `use a::b as c;` under the
/// rename; both are import bindings.
#[test]
fn named_import_and_rename() {
    let (w, k) = fixtures::named();
    let s = solved(&w);
    assert_eq!(s.def(k.root, VAL, "foo"), Some(k.foo));
    assert_eq!(s.def(k.root, VAL, "bar"), Some(k.foo));
    assert_eq!(s.scope(k.root).get(VAL, "bar").map(|b| b.origin), Some(Origin::Import));
    assert!(s.errors(k.root).is_empty(), "{:?}", s.errors(k.root));
}

/// Rule: a glob imports every name its source lets the importer name, and an explicit
/// binding of a name shadows the glob's without error.
#[test]
fn explicit_shadows_glob() {
    let (w, k) = fixtures::shadow();
    let s = solved(&w);
    assert_eq!(s.def(k.root, VAL, "foo"), Some(k.b_foo));
    assert_eq!(s.scope(k.root).get(VAL, "foo").map(|b| b.origin), Some(Origin::Import));
    assert_eq!(s.def(k.root, VAL, "bar"), Some(k.a_bar));
    assert_eq!(s.scope(k.root).get(VAL, "bar").map(|b| b.origin), Some(Origin::Glob));
    assert!(s.errors(k.root).is_empty(), "{:?}", s.errors(k.root));
}

/// Rule: two globs bringing one name with different targets are ambiguous, an error only
/// where the name is used.
#[test]
fn two_globs_ambiguous_only_when_used() {
    let (w, k) = fixtures::two_globs();
    let s = solved(&w);
    let x = s.scope(k.root).get(VAL, "x").copied();
    assert_eq!(x.map(|b| b.state), Some(BindingState::Ambiguous(AmbiguityKind::GlobVsGlob)));
    assert!(s.errors(k.root).is_empty(), "{:?}", s.errors(k.root));
    assert_eq!(
        import_error(s.errors(k.user)),
        Some((0, Unresolved::Ambiguous(AmbiguityKind::GlobVsGlob)))
    );
}

/// Rule: `pub use` re-exports chain: an import of a re-export binds the original target.
#[test]
fn pub_use_chain_across_three_modules() {
    let (w, k) = fixtures::chain();
    let s = solved(&w);
    assert_eq!(s.def(k.root, VAL, "f"), Some(k.f));
    assert_eq!(s.def(k.c, VAL, "f"), Some(k.f));
    assert_eq!(s.scope(k.c).get(VAL, "f").map(|b| b.vis), Some(Visibility::Public));
    assert!(s.errors(k.root).is_empty(), "{:?}", s.errors(k.root));
}

/// Rule: globs that import each other (`use super::*` in a child whose parent has `pub use
/// child::*`) reach the least fixpoint: each binds what the other declares, nothing more, and
/// the two are written as one group naming each other.
#[test]
fn super_glob_cycle_is_solved() {
    let (w, k) = fixtures::super_glob();
    let s = solved(&w);
    assert_eq!(s.def(k.root, VAL, "c"), Some(k.c));
    assert_eq!(s.def(k.child, VAL, "r"), Some(k.r));
    assert_eq!(s.scope(k.root).fellows.as_slice(), &[k.child]);
    assert_eq!(s.scope(k.child).fellows.as_slice(), &[k.root]);
    for m in [k.root, k.child] {
        assert!(s.errors(m).is_empty(), "{:?}", s.errors(m));
        assert!(
            s.by[&m].reads.entries.iter().all(|e| e.read.module() != k.root && e.read.module() != k.child),
            "a fellow's read is in the read list"
        );
    }
}

/// Rule: a glob brings only what the importer may name: a private item is invisible through
/// a glob from outside its module and visible from inside it; an outside glob does not even
/// read the private class.
#[test]
fn private_item_invisible_through_glob_from_outside() {
    let (w, k) = fixtures::private();
    let s = solved(&w);
    assert_eq!(s.def(k.root, VAL, "shown"), Some(k.shown));
    assert_eq!(s.def(k.root, VAL, "hidden"), None);
    assert_eq!(s.def(k.inner, VAL, "hidden"), Some(k.hidden));
    let reads = &s.by[&k.root].reads;
    assert!(reads.contains(&Read::Exports { module: k.a, class: crate::VisibilityClass::Public }));
    assert!(!reads.contains(&Read::Exports { module: k.a, class: crate::VisibilityClass::Restricted(k.a) }));
}

/// Rule: `pub(crate)` is the crate, `pub(super)` and `pub(in path)` the named ancestor; an
/// item so restricted is private to a module outside it, and a `pub(in path)` naming no
/// ancestor is an error.
#[test]
fn pub_crate_and_pub_in_path() {
    let (w, k) = fixtures::restricted();
    let s = solved(&w);
    let vis = |name: N| s.scope(k.b).get(VAL, name).map(|b| b.vis);
    assert_eq!(vis("pc"), Some(Visibility::Crate(crate_key(1))));
    assert_eq!(vis("pa"), Some(Visibility::Restricted(k.a)));
    assert_eq!(vis("ps"), Some(Visibility::Restricted(k.a)));
    assert!(s.errors(k.b).iter().any(|e| matches!(e, ScopeError::RestrictedNotAncestor { .. })));
    assert!(s.def(k.c, VAL, "pc").is_some());
    assert_eq!(s.def(k.c, VAL, "pa"), None);
    assert_eq!(import_error(s.errors(k.c)), Some((2, Unresolved::Private)));
}

/// Rule: the extern prelude names the dependencies at a path's start; `extern crate dep as
/// x;` binds `x` to the dependency's root, and one of no dependency is an error.
#[test]
fn extern_crate_and_extern_prelude() {
    let (w, k) = fixtures::extern_crates();
    let s = solved(&w);
    assert_eq!(s.def(k.root, VAL, "d"), Some(k.d));
    let renamed = s.scope(k.a).get(T, "renamed").copied();
    assert_eq!(renamed.map(|b| b.def), Some(Def::module(k.dep_root)));
    assert_eq!(renamed.map(|b| b.origin), Some(Origin::ExternCrate));
    assert!(s.errors(k.a).iter().any(|e| matches!(e, ScopeError::ExternCrate { name: "nothere" })));
    assert!(s.by[&k.root].reads.contains(&Read::Name { module: k.dep_root, ns: VAL, name: "d" }));
}

/// Rule: the standard prelude is the last lexical layer, and `#[no_implicit_prelude]` takes
/// it (and the extern prelude) away.
#[test]
fn std_prelude_and_no_implicit_prelude() {
    let (w, k) = fixtures::prelude();
    let s = solved(&w);
    assert_eq!(s.def(k.a, T, "Vec"), Some(k.vec_struct));
    assert!(s.errors(k.a).is_empty(), "{:?}", s.errors(k.a));
    assert_eq!(s.def(k.b, T, "Vec"), None);
    assert_eq!(import_error(s.errors(k.b)), Some((0, Unresolved::NotFound)));
}

/// Rule: a `macro_rules!` is in scope from its definition on: a call before it fails, a call
/// after it resolves.
#[test]
fn macro_rules_textual_order() {
    let (w, k) = fixtures::textual();
    let s = solved(&w);
    let errors = s.errors(k.root);
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(matches!(
        errors[0],
        ScopeError::Macro { call, why: Unresolved::NotFound, .. } if call == k.before_call
    ));
    assert!(!errors.iter().any(|e| matches!(e, ScopeError::Macro { call, .. } if *call == k.after_call)));
}

/// Rule: `#[macro_use] mod a;` keeps `a`'s `macro_rules!` in scope after the line, in the
/// module and in the siblings declared after it, not before.
#[test]
fn macro_use_mod_carries_to_later_siblings() {
    let (w, k) = fixtures::macro_use();
    let s = solved(&w);
    assert!(s.errors(k.root).is_empty(), "{:?}", s.errors(k.root));
    assert!(s.errors(k.after).is_empty(), "{:?}", s.errors(k.after));
    assert!(matches!(
        s.errors(k.before),
        [ScopeError::Macro { why: Unresolved::NotFound, .. }]
    ));
    assert!(s.scope(k.root).textual.iter().any(|d| d.def == k.m));
}

/// Rule: a `#[macro_export]` macro is a binding of its crate root's scope, reachable by path
/// from other crates.
#[test]
fn macro_export_at_crate_root() {
    let (w, k) = fixtures::macro_export();
    let s = solved(&w);
    let exp = s.scope(k.dep_root).get(MAC, "exp").copied();
    assert_eq!(exp.map(|b| b.def.key), Some(k.exp));
    assert_eq!(exp.map(|b| b.origin), Some(Origin::MacroExport));
    assert!(s.errors(k.root).is_empty(), "{:?}", s.errors(k.root));
}

/// Rule: an import whose name a pending macro call may still define waits for the expansion,
/// in its own module and in a module importing from it.
#[test]
fn expansion_adds_item_a_pending_import_waits_for() {
    let (w, k) = fixtures::expanded();
    let s = solved(&w);
    assert_eq!(s.def(k.root, VAL, "made"), Some(k.made));
    assert_eq!(s.def(k.root, VAL, "alias"), Some(k.made));
    assert_eq!(s.def(k.user, VAL, "made"), Some(k.made));
    assert!(s.errors(k.root).is_empty(), "{:?}", s.errors(k.root));
    assert!(s.errors(k.user).is_empty(), "{:?}", s.errors(k.user));
}

/// Rule: a name a macro expansion adds may not shadow a glob binding an early resolution of
/// the module already used (rustc's GlobVsExpanded, "time travel").
#[test]
fn expanded_item_shadowing_a_used_glob_is_ambiguous() {
    let (w, k) = fixtures::glob_vs_expanded();
    let s = solved(&w);
    assert_eq!(s.def(k.root, T, "tools"), Some(k.tools_expanded));
    assert!(
        s.errors(k.root).iter().any(|e| matches!(
            e,
            ScopeError::Ambiguous { site: Site::Import(_), kind: AmbiguityKind::GlobVsExpanded, name: "tools", .. }
        )),
        "{:?}",
        s.errors(k.root)
    );
}

/// Rule: a read list holds the reads that found nothing too: a lookup that went past the
/// `#[macro_use]` prelude to the standard prelude depends on the first staying empty.
#[test]
fn negative_reads_are_in_the_read_list() {
    let (w, k) = fixtures::negative();
    let s = solved(&w);
    assert!(s.errors(k.root).is_empty(), "{:?}", s.errors(k.root));
    let reads = &s.by[&k.root].reads;
    let absent = Read::Name { module: k.dep_root, ns: MAC, name: "pm" };
    assert!(
        reads.entries.iter().any(|e| e.read == absent && e.saw == Saw::Name(NameAnswer::Absent)),
        "{:?}",
        reads.entries
    );
    assert!(reads.contains(&Read::Name { module: k.prelude, ns: MAC, name: "pm" }));
}
