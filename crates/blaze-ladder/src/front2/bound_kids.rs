//! The written types inside an `impl Trait` parameter's bound path, for the walk that numbers
//! a fn's anonymous type parameters (heads.rs `anon_params`).
//!
//! The Reference, "Trait and lifetime bounds" (associated item constraints, RFC 2289, stable
//! since 1.79): `impl Tr<Assoc: Bounds>` is `impl Tr` and `<P as Tr>::Assoc: Bounds`, `P` the
//! anonymous type parameter. Such a constraint is no type argument and holds no type of the
//! path's own: its bounds are lowered as clauses on the projection by preds.rs
//! `Builder::bound` (its `nested` work list), never as types here. So the walk passes over it.
//! tempo writes `mut db: impl Database<Error: Into<EthApiError>>` (reth's `Call`) and
//! `transactions: impl IntoIterator<Item = (..), IntoIter: Send> + Send` (reth's
//! `TransactionValidator::validate_transactions`).
//!
//! Before this walk the bound's path was pushed to the type walk as a TYPE, whose children
//! (types.rs `children`, `arg_types`) refuse a constraint ("an associated item constraint"):
//! right for a path in a type position (`Vec<A: B>` is the language's E0229, "associated item
//! constraints are not allowed here"), wrong for a trait path in a bound.
//!
//! The order is the old walk's: every segment's type arguments in writing order, then the
//! types of the bindings (`Name = T`) of the segments that are not parenthesized; a binding is
//! written after every argument of its segment and only on a trait path's last segment, so
//! this is text order. An `impl` inside an associated type bound (`Item: Into<impl X>`) is not
//! walked to: it stays refused where made ("an `impl Trait` type outside a fn's parameters and
//! return type"). Two passes over the segments, no allocation.

use blaze_lower::{View, form as lf};

use super::types::Why;

/// The types written in the bound path `ty` of an `impl Trait` parameter, in text order, into
/// `out`: type arguments, then binding types; an associated type bound (`Name: Bounds`), a
/// lifetime and a const argument are passed over (each is lowered by `Builder::bound`). An
/// associated const binding (`Tr<N = 3>`) and a qualified path as a bound's trait are refused
/// by name. `false` when the bound is no path: the caller walks it as a type, as before.
pub(super) fn bound_path_kids(v: &View<'_>, ty: lf::TypeId, out: &mut impl Extend<lf::TypeId>) -> Result<bool, Why> {
    let lf::TypeRef::Path(p) = v.ty(ty)? else { return Ok(false) };
    let r = v.site(v.type_range(ty)?)?;
    let segments = v.segments(v.path(p)?)?;
    // Pass 0: type arguments; pass 1: binding types.
    for pass in 0..2u8 {
        for &n in segments {
            let s = lf::Segment::decode(n)?;
            if s.kind == lf::SegKind::Anchor {
                return Err(Why::Unsupported("a qualified path as an `impl Trait` bound's trait (`impl <T as Tr>::X`)", r));
            }
            if !s.has_args {
                continue;
            }
            for &a in v.rows(s.args)? {
                match lf::GenericArg::decode(a)? {
                    lf::GenericArg::Type(t) if pass == 0 => out.extend([t]),
                    lf::GenericArg::AssocType { ty: bound_ty, .. } if pass == 1 && !s.parenthesized => out.extend([bound_ty]),
                    lf::GenericArg::AssocConst { .. } => {
                        return Err(Why::Unsupported("an associated const binding in a bound (`Tr<N = 3>`)", r));
                    }
                    lf::GenericArg::Type(_) | lf::GenericArg::AssocType { .. } | lf::GenericArg::Lifetime(_) | lf::GenericArg::Const(_) | lf::GenericArg::AssocBound { .. } => {}
                }
            }
        }
    }
    Ok(true)
}
