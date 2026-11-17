//! blaze: error unions (`#![blaze::error_unions]`) in the solver.
//!
//! `A | B` is core's `ErrorUnion<(A, B)>`, its members in one canonical order. The builtin
//! candidate of `U: UnionFrom<S>` (core's `error_union::UnionFrom`) holds when `U` is an error
//! union and `S` is one of its members, or an error union of some of its members but not all
//! (then `S` is `U`, which `From<T> for T` covers). Its associated type `Path` says where each
//! value of `S` goes in `U`: for a member, the index of the member it is, written as nested
//! one-tuples around `()` (`0` is `()`, `1` is `((),)`); for a union, the tuple of its members'
//! paths. Core's `Inject` impls are written over those paths, so the conversion itself is
//! ordinary library code.
//!
//! A member is found by the types alone ([`DeepRejectCtxt`], regions ignored), then related, so
//! a member with lifetimes gets its region constraints. A source that may be more than one
//! member (it still has inference variables) is ambiguous.

use rustc_type_ir::fast_reject::DeepRejectCtxt;
use rustc_type_ir::inherent::*;
use rustc_type_ir::lang_items::SolverAdtLangItem;
use rustc_type_ir::{self as ty, Interner};

use crate::delegate::SolverDelegate;
use crate::solve::{EvalCtxt, NoSolution, NoSolutionOrRerunNonErased};

/// What `U: UnionFrom<S>` comes to, once it is known not to fail.
pub(super) enum UnionFrom<I: Interner> {
    /// It holds, and this is its `Path`.
    Holds(I::Ty),
    /// `S`, or a member of it, could still be more than one member of `U`.
    Ambiguous,
}

/// The members of `ty` when it is an error union whose members are known, in canonical order.
pub(super) fn error_union_members<I: Interner>(cx: I, ty: I::Ty) -> Option<I::Tys> {
    let ty::Adt(def, args) = ty.kind() else { return None };
    if cx.as_adt_lang_item(def.def_id()) != Some(SolverAdtLangItem::ErrorUnion) {
        return None;
    }
    match args.type_at(0).kind() {
        ty::Tuple(members) => Some(members),
        _ => None,
    }
}

/// The `Path` of the member at `at`: `at` one-tuples around `()`.
fn member_path<I: Interner>(cx: I, at: usize) -> I::Ty {
    let mut path = I::Ty::new_tup(cx, &[]);
    for _ in 0..at {
        path = I::Ty::new_tup(cx, &[path]);
    }
    path
}

impl<D, I> EvalCtxt<'_, D>
where
    D: SolverDelegate<Interner = I>,
    I: Interner,
{
    /// `U: UnionFrom<source>` for the error union `U` of `members`, inside the candidate's
    /// probe: the source's member (or each member of a source union) is related to the one it
    /// is in `U`.
    pub(super) fn error_union_from(
        &mut self,
        param_env: I::ParamEnv,
        members: I::Tys,
        source: I::Ty,
    ) -> Result<UnionFrom<I>, NoSolutionOrRerunNonErased> {
        let cx = self.cx();
        let source = self.structurally_normalize_ty(param_env, source)?;
        if let ty::Infer(ty::TyVar(_)) = source.kind() {
            return Ok(UnionFrom::Ambiguous);
        }
        let Some(sources) = error_union_members(cx, source) else {
            return self.error_union_member(param_env, members, source);
        };
        // Members are distinct, so as many or more is the union itself or not a subset.
        if sources.len() >= members.len() {
            return Err(NoSolution.into());
        }
        // alloc: at most 11 paths, for the one tuple they are interned into
        let mut paths = Vec::with_capacity(sources.len());
        for member in sources.iter() {
            match self.error_union_member(param_env, members, member)? {
                UnionFrom::Holds(path) => paths.push(path),
                UnionFrom::Ambiguous => return Ok(UnionFrom::Ambiguous),
            }
        }
        Ok(UnionFrom::Holds(I::Ty::new_tup(cx, &paths)))
    }

    /// The path of `source`, one type, among `members`.
    fn error_union_member(
        &mut self,
        param_env: I::ParamEnv,
        members: I::Tys,
        source: I::Ty,
    ) -> Result<UnionFrom<I>, NoSolutionOrRerunNonErased> {
        // Parameters are rigid (a member never has one); inference variables may be anything.
        let reject = DeepRejectCtxt::relate_rigid_rigid(self.cx());
        let mut found = None;
        for (at, member) in members.iter().enumerate() {
            if !reject.types_may_unify(source, member) {
                continue;
            }
            if found.is_some() {
                return Ok(UnionFrom::Ambiguous);
            }
            found = Some((at, member));
        }
        let Some((at, member)) = found else { return Err(NoSolution.into()) };
        self.eq(param_env, source, member)?;
        Ok(UnionFrom::Holds(member_path(self.cx(), at)))
    }
}
