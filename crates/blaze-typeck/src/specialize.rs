//! Specialization at a ground selection: among the impls that hold, the most specific one is
//! the one used (walk list-AJ, P9).
//!
//! The pinned alloc enables `min_specialization` and writes, in library/alloc/src/string.rs,
//! `impl<T: fmt::Display + ?Sized> ToString for T { default fn to_string(&self) .. }` beside
//! `impl ToString for str` (and `String`, `char`, `bool`, `u8`, `i8`, `Cow<'_, str>`,
//! `fmt::Arguments<'_>`); core's library/core/src/fmt/mod.rs has `impl<W: Write + ?Sized>
//! SpecWriteFmt for &mut W { default fn spec_write_fmt .. }` beside `impl<W: Write>
//! SpecWriteFmt for &mut W`. At an instance (`<str as ToString>::to_string`, `write_fmt` at a
//! sized writer) both impls hold.
//!
//! The rule (specialization is unstable, so RFC 1210 and the library's own feature are the
//! evidence): impl A specializes impl B when every trait reference A applies to, B applies to:
//! A's trait reference, its parameters left rigid, is an instance of B's (B's header unifies
//! with it, B's parameters fresh variables), and B's where-clauses hold under A's where-clauses
//! as assumptions. At a ground selection the impl that specializes every other holding impl is
//! chosen; when none does (or two do, which only identical headers can), the selection is the
//! `Overlap` it always was. Whether B's item is `default` is the definition check's question
//! (a program whose specializing impl overrides a non-`default` item is refused there), not the
//! instance's.

use blaze_own::{Alloc, Vec};

use crate::error::CheckError;
use crate::ids::ItemKey;
use crate::infer::InferCtx;
use crate::instance::{NoClosures, ResolveError, import, import_list, unify_header};
use crate::portable::PTys;
use crate::ty::{Cause, Goal, Obligation, Ty, TypeCtx};
use crate::world::{ImplHeader, PClause, Question, Reads, World};

/// Where-clause `c` of a record in `types`, imported with `Param(i)` as `subst[i]` (a
/// parameter past `subst` stays a rigid `TyView::Param(i)`), as a goal; `None` for an outlives
/// clause, which is the borrow checker's (D148) and taken as true here.
pub(crate) fn clause_goal(cx: &mut InferCtx<'_>, types: PTys<'_>, c: PClause, subst: &[Ty]) -> Result<Option<Goal>, CheckError> {
    Ok(Some(match c {
        PClause::Trait { trait_, args } => Goal::Trait { trait_, args: import_list(cx, &mut NoClosures, types, args, subst, 0)? },
        PClause::Projection { trait_, args, name, term } => Goal::Projection {
            trait_,
            args: import_list(cx, &mut NoClosures, types, args, subst, 0)?,
            name,
            term: import(cx, &mut NoClosures, types, term, subst, 0)?,
        },
        PClause::Const { cond, args } => Goal::ConstHolds { cond, args: import_list(cx, &mut NoClosures, types, args, subst, 0)? },
        PClause::ForFields { container, trait_, args } => Goal::ForFields {
            container: import(cx, &mut NoClosures, types, container, subst, 0)?,
            trait_,
            args: import_list(cx, &mut NoClosures, types, args, subst, 0)?,
        },
        PClause::TypeOutlives { .. } | PClause::RegionOutlives { .. } => return Ok(None),
    }))
}

/// Whether impl `a` specializes impl `b` (both impls of `trait_`): A's trait reference with
/// its own parameters rigid, B's header unified against it with fresh variables by kind, B's
/// where-clauses proved with A's assumed. Everything is done inside one snapshot, always rolled
/// back: `cx` is as it was on return, whatever the answer.
pub(crate) fn specializes<W: World>(
    cx: &mut InferCtx<'_>,
    reads: &mut Reads<'_, '_, W>,
    a: ItemKey,
    b: ItemKey,
    trait_: ItemKey,
) -> Result<bool, ResolveError> {
    let Some(ha) = reads.impl_header(a)? else { return Err(ResolveError::Missing(Question::ImplHeader(a))) };
    let Some(hb) = reads.impl_header(b)? else { return Err(ResolveError::Missing(Question::ImplHeader(b))) };
    // Both came out of the selection of `trait_`; anything else is a caller's slip, refused by
    // name rather than answered.
    if ha.trait_ != Some(trait_) {
        return Err(ResolveError::NoImpl { trait_, item: a });
    }
    if hb.trait_ != Some(trait_) {
        return Err(ResolveError::NoImpl { trait_, item: b });
    }
    let s = cx.snapshot();
    let r = specializes_in(cx, reads, a, &ha, b, &hb);
    cx.rollback_to(s);
    r
}

fn specializes_in<W: World>(
    cx: &mut InferCtx<'_>,
    reads: &mut Reads<'_, '_, W>,
    a: ItemKey,
    ha: &ImplHeader<'_>,
    b: ItemKey,
    hb: &ImplHeader<'_>,
) -> Result<bool, ResolveError> {
    // A's trait reference, self first, imported with an empty substitution: its parameters
    // stay rigid `Param(i)` (and `ConstParam(i)`), as in A's own definition check.
    let rest = ha.types.list(ha.trait_args.list())?;
    let mut a_args: Vec<Ty, Alloc<'_>> = Vec::with_capacity_in(rest.len() + 1, cx.alloc());
    a_args.push(import(cx, &mut NoClosures, ha.types, ha.self_ty, &[], 0)?);
    for &p in rest {
        a_args.push(import(cx, &mut NoClosures, ha.types, p, &[], 0)?);
    }
    let a_ref = cx.mk_list(&a_args)?;
    // A's where-clauses (its implied sizedness included) are the param env.
    let wa = reads.predicates(a)?;
    for &c in wa.clauses {
        let Some(goal) = clause_goal(cx, wa.types, c, &[])? else { continue };
        cx.assume(goal)?;
    }
    // B's header, its parameters fresh variables by kind, against A's trait reference.
    let Some(fresh) = unify_header(cx, hb, a_ref)? else { return Ok(false) };
    let wb = reads.predicates(b)?;
    for &c in wb.clauses {
        let Some(goal) = clause_goal(cx, wb.types, c, &fresh)? else { continue };
        cx.register(Obligation { goal, cause: Cause::Body })?;
    }
    cx.select_all(reads)?;
    Ok(cx.next_error().is_none())
}

/// Of the impls of `trait_` that hold (`holding`, in selection order, at least one), the one
/// that specializes every other; `Overlap` naming two of them when none does, or when a
/// second one does too.
pub(crate) fn most_specific<W: World>(
    cx: &mut InferCtx<'_>,
    reads: &mut Reads<'_, '_, W>,
    trait_: ItemKey,
    holding: &[ItemKey],
) -> Result<usize, ResolveError> {
    let (Some(&first), Some(&second)) = (holding.first(), holding.get(1)) else {
        return match holding.first() {
            Some(_) => Ok(0),
            None => Err(ResolveError::NotGround { item: trait_ }),
        };
    };
    let mut winner: Option<usize> = None;
    for (i, &a) in holding.iter().enumerate() {
        let mut all = true;
        for (j, &b) in holding.iter().enumerate() {
            if i == j {
                continue;
            }
            if !specializes(cx, reads, a, b, trait_)? {
                all = false;
                break;
            }
        }
        if !all {
            continue;
        }
        if let Some(w) = winner {
            return Err(ResolveError::Overlap { trait_, first: holding[w], second: a });
        }
        winner = Some(i);
    }
    winner.ok_or(ResolveError::Overlap { trait_, first, second })
}
