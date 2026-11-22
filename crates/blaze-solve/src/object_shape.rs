//! The shape impls are filed and asked by, with a trait object's principal (x0-dyn-04,
//! 2026-11-16).
//!
//! [`Shape::Dynamic`] says "`dyn Trait`: the principal, or 0 for `dyn Send` and the like", and
//! every impl table the solver reads (blaze-typeck's fixture, front2's buckets, `instance.rs`)
//! files `impl Other for dyn Tr` under `Dynamic(Tr)`. [`shape_of_kind`] sees one level only and
//! cannot read the predicate list, so it answered `Dynamic(0)` for every object: the solver
//! asked the impls of `Other` at `Dynamic(0)`, the table answered the impls written for
//! principal-less objects, and `dyn Tr: Other` failed although `impl Other for dyn Tr` holds
//! (E0277 for `Other::o(t)`, and E0599 for `t.o()`, whose probe asks the same goal).
//!
//! The Reference, "Implementations": "An implementation is an item that associates items with
//! an implementing type"; "Trait object types": a trait object type is a type like any other,
//! so `impl Other for dyn Tr` is an impl whose implementing type is `dyn Tr`, and the goal
//! `dyn Tr: Other` has it as a candidate. Every place that asks or files impls by shape takes
//! the shape from [`shape_with_principal`] (one home: the principal is the FIRST predicate of
//! a `Dynamic` list, term.rs `TyKind::Dynamic`: "`ExTrait` principal first").

use crate::term::{Def, List, Shape, Ty, TyKind, shape_of_kind};
use crate::tys::Tys;

/// The shape of a term of kind `k`, as [`shape_of_kind`], except that an object's shape names
/// its principal trait: `first(preds)` is the kind of the object's first predicate (`None` for
/// an empty list), read from whichever arena `k` came from (the solver's terms, or an impl
/// header's portable nodes). An object with no principal (`dyn Send`) is `Dynamic(Def(0))`.
pub fn shape_with_principal(k: TyKind, first: impl FnOnce(List) -> Option<TyKind>) -> Option<Shape> {
    match k {
        TyKind::Dynamic(preds, _) => Some(Shape::Dynamic(match first(preds) {
            Some(TyKind::ExTrait(d, _)) => d,
            _ => Def(0),
        })),
        _ => shape_of_kind(k),
    }
}

/// The shape of the solver's term `t` after resolving its top ([`shape_with_principal`] over
/// `tys`): what a goal about `t` asks the impl table for.
pub fn shape_of_ty<T: Tys>(tys: &T, t: Ty) -> Option<Shape> {
    let k = tys.kind(tys.shallow_resolve(t));
    shape_with_principal(k, |l| if l.len > 0 { Some(tys.kind(tys.list_get(l, 0))) } else { None })
}
