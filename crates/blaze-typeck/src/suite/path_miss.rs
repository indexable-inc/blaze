//! E0425 names the segment that failed (rule 19), not the path's last one (rules/lit_path.rs
//! `unresolved`, `path_miss`; the world's answer [`crate::PathMiss`]).
//!
//! The Reference, "Paths", "Paths in expressions": a path is resolved segment by segment, the
//! first in the body's scope, each next one among the items (or associated items) of what the
//! one before it named. So `nope::new` with no `nope` in scope fails at `nope`, and `Tr::missing`
//! with a trait `Tr` that has no `missing` fails at `missing`, after `Tr`.
//!
//! Before the fix every E0425 from a value path carried the LAST segment's name and no reason:
//! front2's finding text was "found None, expected None, name Some(Name(..))" for `Rc::new`,
//! the same whether `Rc` or `new` was the one missing.
//!
//! The world (the fixture's, which resolves the longest prefix of the written names):
//! ```text
//! #[lang = "sized"] trait Sized {}
//! trait Tr {}
//! struct Rc {}
//! fn g() { <path> }
//! ```

use std::vec::Vec;

use super::check_body;
use crate::diag::{Because, DiagCode};
use crate::fixture::{Bb, Fx};
use crate::ids::{ItemKey, LangItem, Name};
use crate::infer::InferPlant;
use crate::machine::Plant;
use crate::path_miss::{MissWhy, PathMiss, PathPrefix};
use crate::world::Def;

/// `fn g() { <segments> }` in the module doc's world, and the trait's and the struct's keys.
fn check_path(segments: &[&str]) -> (crate::Checked<'static>, ItemKey, ItemKey) {
    let mut fx = Fx::new();
    let sized = fx.tr("Sized", 1, Vec::new(), &[]);
    fx.lang(LangItem::Sized, sized);
    let tr = fx.tr("Tr", 1, Vec::new(), &[]);
    let rc = fx.strukt("Rc", 0, &[]);
    let unit = fx.unit();
    let owner = fx.func("g", 0, &[], unit, Vec::new());
    fx.finish();
    let mut bb = Bb::new();
    let p = bb.path(segments);
    let root = bb.block(&[], Some(p));
    (check_body(&fx, &bb, owner, root, Plant::None, InferPlant::None), tr, rc)
}

/// The E0425 findings of a check: each one's name and reason.
fn e0425(c: &crate::Checked<'_>) -> Vec<(Option<Name>, Option<Because>)> {
    c.diags.iter().filter(|d| d.code == DiagCode::UnresolvedPath).map(|d| (d.name, d.because)).collect()
}

/// Prints nothing; fails with "nope::new: [..]" when the finding names `new` (the last
/// segment, as before the fix) or carries no `Because::Path`.
#[test]
fn a_missing_first_segment_is_the_one_named() {
    let (c, _, _) = check_path(&["nope", "new"]);
    let found = e0425(&c);
    let want = PathMiss::At { segment: 0, after: None, why: MissWhy::NotFound };
    assert_eq!(found, [(Some(Name::of("nope")), Some(Because::Path(want)))], "nope::new: {:?}", c.diags);
}

/// Prints nothing; fails with "Tr::missing: [..]" when the finding does not say segment 1
/// failed after the trait `Tr`. And `Rc::new` on a struct with no `new` is not an E0425 at all:
/// the struct resolved, so it is E0599 naming `new` with the struct as the type found
/// (rules/lit_path.rs `assoc_value`), the type and the segment both named.
#[test]
fn a_missing_last_segment_after_a_found_type_names_both() {
    let (c, tr, _) = check_path(&["Tr", "missing"]);
    let want = PathMiss::At { segment: 1, after: Some(PathPrefix::Def(Def::Trait(tr))), why: MissWhy::NotFound };
    assert_eq!(e0425(&c), [(Some(Name::of("missing")), Some(Because::Path(want)))], "Tr::missing: {:?}", c.diags);

    let (c, _, rc) = check_path(&["Rc", "new"]);
    assert!(e0425(&c).is_empty(), "Rc::new is no E0425: {:?}", c.diags);
    let no_method: Vec<_> = c.diags.iter().filter(|d| d.code == DiagCode::NoMethod).map(|d| d.name).collect();
    assert_eq!(no_method, [Some(Name::of("new"))], "Rc::new: {:?}", c.diags);
    assert!(c.diags.iter().any(|d| d.code == DiagCode::NoMethod && d.found.is_some()), "Rc::new names no type: {:?} (Rc is {rc:?})", c.diags);
}

/// CONTROL: a one-segment path's failing segment is its last, so the name is the one the
/// checker gave before the fix (`missing`); only the reason is new. Prints nothing.
#[test]
fn control_a_one_segment_miss_names_the_same_segment_as_before() {
    let (c, _, _) = check_path(&["missing"]);
    let want = PathMiss::At { segment: 0, after: None, why: MissWhy::NotFound };
    assert_eq!(e0425(&c), [(Some(Name::of("missing")), Some(Because::Path(want)))], "missing: {:?}", c.diags);
}
