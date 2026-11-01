//! Coercion by head dispatch: rustc's rules in rustc's order, with the snapshots and trials
//! that cannot change the outcome left out.
//!
//! rustc tries, in order: `!` to anything (R1); from an inference variable (R2); unsizing in a
//! snapshot, falling back on failure (R3); then exactly one of a raw pointer target (R4), a
//! reference target (R5), a function item (R6), function pointer (R7) or closure (R8) source,
//! or subtyping (R9). [`Coerce::head`] picks R1, R2, R3-then-the-rest, or the rest alone from
//! the two kind tags, and R3 is skipped only where it can only fail:
//!
//! - R3's own rejections before it writes anything (a variable target, a target that is never
//!   `CoerceUnsized`'s right side, `&str` to `&str`, the missing lang items, the reborrow's
//!   mutability), checked before any snapshot;
//! - a source with no `CoerceUnsized` candidate: the solver has no builtin candidate for the
//!   trait, so with no impl for the source's head, no such clause in the param env, a source
//!   that is no alias, `dyn`, parameter or reference and no error, the goal fails with no
//!   candidate and R3's visitor breaks. The target has no alias, so relating it to R3's fresh
//!   variable registers no goal that could leave anything behind the rollback;
//! - a failure of R3 for the same two types in the same body ([`UnsizeMemo`]).
//!
//! The root snapshot (rustc's `commit_if_ok` around `coerce`) is taken only where a rule may
//! fail after a write outside its own snapshots: R3, and R5 from a reference, whose autoderef
//! makes variables. Every other write is inside `unify_raw`'s snapshot or cannot fail. A
//! snapshot whose failure always reaches one further out, or a context that is thrown away, is
//! left out (`rule_snapshot`): rolling back once restores what rolling back twice does.

use blaze_hash::FixedHasher;
use rustc_type_ir::{
    Interner as _, TypeVisitableExt, fold_regions,
    inherent::{Clause as _, IntoKind, Ty as _},
};
use std::hash::{Hash, Hasher};
use tracing::debug;

use super::{
    Adjust, Adjustment, Coerce, CoerceDelegate, CoerceResult, DbInterner, InferOk, Mutability,
    PredicateObligations, TraitRef, Ty, TyKind, TypeError, coerce_mutbls, success,
};
use crate::next_solver::{Region, RegionExt, RegionKind};

/// Which of rustc's rules a coercion of the two (shallowly resolved) types goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Head {
    /// R1.
    Never,
    /// R2.
    FromInfer,
    /// R3 in its snapshot, then R4 to R9 if it fails.
    Unsize,
    /// R4 to R9: R3 can only fail.
    Direct,
}

/// Why R3 failed: whether the outcome read the inference table beyond the two types, which
/// keeps it out of the [`UnsizeMemo`].
#[derive(Clone, Copy, Debug)]
pub(super) struct UnsizeFailed {
    pub(super) read_table: bool,
}

impl<'db> From<TypeError<DbInterner<'db>>> for UnsizeFailed {
    fn from(_: TypeError<DbInterner<'db>>) -> Self {
        UnsizeFailed { read_table: false }
    }
}

pub(super) type UnsizeResult<'db, 'f> = Result<
    InferOk<'db, (Vec<Adjustment, &'f blaze_mem::Frame>, Ty<'db>)>,
    UnsizeFailed,
>;

const SLOTS: usize = 32;

/// The pairs of types R3 failed for in this body, by `use_lub`: direct-mapped, 512 bytes, no
/// allocation. Only pairs with no non-region inference variable, error or alias go in, with
/// their region variables erased: R3's outcome is then a function of the two types up to
/// regions (the solver canonicalizes every region to a fresh variable, and region relations do
/// not fail in typeck), the param env and the typing mode, which are the body's.
#[derive(Clone, Copy, Debug)]
pub(crate) struct UnsizeMemo<'db> {
    failed: [[Option<(Ty<'db>, Ty<'db>)>; SLOTS]; 2],
}

impl Default for UnsizeMemo<'_> {
    fn default() -> Self {
        UnsizeMemo { failed: [[None; SLOTS]; 2] }
    }
}

impl<'db> UnsizeMemo<'db> {
    #[inline]
    fn slot(key: (Ty<'db>, Ty<'db>)) -> usize {
        let mut hasher = FixedHasher::default();
        key.hash(&mut hasher);
        (hasher.finish() >> (64 - SLOTS.trailing_zeros())) as usize
    }

    #[inline]
    fn failed(&self, use_lub: bool, key: (Ty<'db>, Ty<'db>)) -> bool {
        self.failed[use_lub as usize][Self::slot(key)] == Some(key)
    }

    #[inline]
    fn remember(&mut self, use_lub: bool, key: (Ty<'db>, Ty<'db>)) {
        self.failed[use_lub as usize][Self::slot(key)] = Some(key);
    }
}

/// Whether the shortcuts are on: off only for the old order in the differential test.
#[inline(always)]
pub(super) fn shortcuts() -> bool {
    #[cfg(test)]
    {
        !tests::old_coercion()
    }
    #[cfg(not(test))]
    {
        true
    }
}

impl Head {
    fn needs_root_snapshot(self, a: Ty<'_>, b: Ty<'_>) -> bool {
        if !shortcuts() {
            return true;
        }
        match self {
            Head::Never | Head::FromInfer => false,
            Head::Unsize => true,
            Head::Direct => matches!((a.kind(), b.kind()), (TyKind::Ref(..), TyKind::Ref(..))),
        }
    }
}

impl<'db, 'f, D> Coerce<'f, D>
where
    D: CoerceDelegate<'db>,
{
    /// A coercion at the root, where rustc takes a snapshot around [`Coerce::coerce`].
    pub(super) fn coerce_at_root(&mut self, a: Ty<'db>, b: Ty<'db>) -> CoerceResult<'db, 'f> {
        let a = self.infcx().shallow_resolve(a);
        let b = self.infcx().shallow_resolve(b);
        let head = self.head(a, b);
        if head.needs_root_snapshot(a, b) {
            self.commit_if_ok(|this| this.coerce_head(a, b, head))
        } else {
            self.coerce_head(a, b, head)
        }
    }

    pub(super) fn head(&mut self, a: Ty<'db>, b: Ty<'db>) -> Head {
        if a.is_never() {
            return Head::Never;
        }
        if a.is_infer() {
            return Head::FromInfer;
        }
        if !shortcuts() {
            return Head::Unsize;
        }
        if self.unsize_rejected_up_front(a, b) || self.unsize_known_to_fail(a, b) {
            Head::Direct
        } else {
            Head::Unsize
        }
    }

    pub(super) fn coerce_head(
        &mut self,
        a: Ty<'db>,
        b: Ty<'db>,
        head: Head,
    ) -> CoerceResult<'db, 'f> {
        match head {
            Head::Never => self.coerce_from_never(a, b),
            Head::FromInfer => self.coerce_from_inference_variable(a, b),
            Head::Unsize => match self.try_unsize(a, b) {
                Ok(ok) => {
                    debug!("coerce: unsize successful");
                    Ok(ok)
                }
                Err(failed) => {
                    debug!(?failed, "coerce: unsize failed");
                    self.coerce_direct(a, b)
                }
            },
            Head::Direct => self.coerce_direct(a, b),
        }
    }

    fn coerce_from_never(&mut self, a: Ty<'db>, b: Ty<'db>) -> CoerceResult<'db, 'f> {
        // If we're coercing into an inference var, mark it as possibly diverging.
        if b.is_infer() {
            self.delegate.set_diverging(b);
        }
        if self.coerce_never {
            let mut adjustments = Vec::with_capacity_in(1, self.frame);
            adjustments.push(Adjustment { kind: Adjust::NeverToAny, target: b.store() });
            success(adjustments, b, PredicateObligations::new())
        } else {
            // Otherwise the only coercion we can do is unification.
            self.unify(a, b)
        }
    }

    /// R3 in its own snapshot: it makes a type variable for the unsized target, which must not
    /// stay if the coercion fails.
    fn try_unsize(&mut self, a: Ty<'db>, b: Ty<'db>) -> UnsizeResult<'db, 'f> {
        if !shortcuts() {
            return self.commit_if_ok(|this| {
                if this.unsize_rejected_up_front(a, b) {
                    return Err(TypeError::Mismatch.into());
                }
                this.coerce_unsized(a, b)
            });
        }
        let result = self.commit_if_ok(|this| this.coerce_unsized(a, b));
        if let Err(UnsizeFailed { read_table: false }) = result
            && let Some(key) = self.unsize_key(a, b)
        {
            let use_lub = self.use_lub;
            if let Some(memo) = self.delegate.unsize_memo() {
                memo.remember(use_lub, key);
            }
        }
        result
    }

    /// R4 to R9: the rule the target's or else the source's kind selects.
    fn coerce_direct(&mut self, a: Ty<'db>, b: Ty<'db>) -> CoerceResult<'db, 'f> {
        // Examine the supertype and consider type-specific coercions, such
        // as auto-borrowing, coercing pointer mutability, a `dyn*` coercion,
        // or pin-ergonomics.
        match b.kind() {
            TyKind::RawPtr(_, b_mutbl) => return self.coerce_raw_ptr(a, b, b_mutbl),
            TyKind::Ref(r_b, _, mutbl_b) => {
                return self.coerce_borrowed_pointer(a, b, r_b, mutbl_b);
            }
            _ => {}
        }

        match a.kind() {
            // Function items are coercible to any closure type; function pointers are not
            // (that would require double indirection). Additionally, we permit coercion of
            // function items to drop the unsafe qualifier.
            TyKind::FnDef(..) => self.coerce_from_fn_item(a, b),
            // We permit coercion of fn pointers to drop the unsafe qualifier.
            TyKind::FnPtr(a_sig_tys, a_hdr) => self.coerce_from_fn_pointer(a_sig_tys.with(a_hdr), b),
            // Non-capturing closures are coercible to function pointers or unsafe function
            // pointers. It cannot convert closures that require unsafe.
            TyKind::Closure(closure_def_id_a, args_a) => {
                self.coerce_closure_to_fn(a, closure_def_id_a.0, args_a, b)
            }
            // Otherwise, just use unification rules.
            _ => self.unify(a, b),
        }
    }

    /// rustc's snapshot around a rule whose failure always reaches a rollback further out (the
    /// root snapshot, `may_coerce`'s, or a context that is thrown away); taken only by the old
    /// order.
    pub(super) fn rule_snapshot<T, E>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T, E>,
    ) -> Result<T, E> {
        if shortcuts() { f(self) } else { self.commit_if_ok(f) }
    }

    /// R3's checks that need no inference: `Err` from any of them makes R3 fail with nothing
    /// written.
    fn unsize_rejected_up_front(&self, source: Ty<'db>, target: Ty<'db>) -> bool {
        // We don't apply any coercions incase either the source or target
        // aren't sufficiently well known but tend to instead just equate
        // them both.
        if source.is_infer() || target.is_infer() {
            return true;
        }

        // These targets are known to never be RHS in `LHS: CoerceUnsized<RHS>`.
        // That's because these are built-in types for which a core-provided impl
        // doesn't exist, and for which a user-written impl is invalid.
        //
        // This is technically incomplete when users write impossible bounds like
        // `where T: CoerceUnsized<usize>`, for example, but that trait is unstable
        // and coercion is allowed to be incomplete. The only case where this matters
        // is impossible bounds.
        //
        // Note that some of these types implement `LHS: Unsize<RHS>`, but they
        // do not implement *`CoerceUnsized`* which is the root obligation of the
        // check below.
        if let TyKind::Bool
        | TyKind::Char
        | TyKind::Int(_)
        | TyKind::Uint(_)
        | TyKind::Float(_)
        | TyKind::Infer(rustc_type_ir::IntVar(_) | rustc_type_ir::FloatVar(_))
        | TyKind::Str
        | TyKind::Array(_, _)
        | TyKind::Slice(_)
        | TyKind::FnDef(_, _)
        | TyKind::FnPtr(_, _)
        | TyKind::Dynamic(_, _)
        | TyKind::Closure(_, _)
        | TyKind::CoroutineClosure(_, _)
        | TyKind::Coroutine(_, _)
        | TyKind::CoroutineWitness(_, _)
        | TyKind::Never
        | TyKind::Tuple(_) = target.kind()
        {
            return true;
        }

        // Additionally, we ignore `&str -> &str` coercions, which happen very
        // commonly since strings are one of the most used argument types in Rust,
        // we do coercions when type checking call expressions.
        if let TyKind::Ref(_, source_pointee, Mutability::Not) = source.kind()
            && source_pointee.is_str()
            && let TyKind::Ref(_, target_pointee, Mutability::Not) = target.kind()
            && target_pointee.is_str()
        {
            return true;
        }

        let lang_items = self.interner().lang_items();
        if lang_items.Unsize.is_none() || lang_items.CoerceUnsized.is_none() {
            debug!("missing Unsize or CoerceUnsized traits");
            return true;
        }

        // The reborrow R3 adds before selecting `Source: CoerceUnsized<Target>`.
        match (source.kind(), target.kind()) {
            (TyKind::Ref(_, _, mutbl_a), TyKind::Ref(_, _, mutbl_b))
            | (TyKind::Ref(_, _, mutbl_a), TyKind::RawPtr(_, mutbl_b)) => {
                coerce_mutbls(mutbl_a, mutbl_b).is_err()
            }
            _ => false,
        }
    }

    /// Whether R3 is known to fail: it failed for the same types in this body, or no candidate
    /// for `source: CoerceUnsized<_>` exists.
    fn unsize_known_to_fail(&mut self, source: Ty<'db>, target: Ty<'db>) -> bool {
        let use_lub = self.use_lub;
        let key = self.unsize_key(source, target);
        if let Some(key) = key
            && let Some(memo) = self.delegate.unsize_memo()
            && memo.failed(use_lub, key)
        {
            return true;
        }
        if !self.unsize_has_no_candidate(source, target) {
            return false;
        }
        if let Some(key) = key
            && let Some(memo) = self.delegate.unsize_memo()
        {
            memo.remember(use_lub, key);
        }
        true
    }

    fn unsize_has_no_candidate(&self, source: Ty<'db>, target: Ty<'db>) -> bool {
        let source_has_only_impls = matches!(
            source.kind(),
            TyKind::Bool
                | TyKind::Char
                | TyKind::Int(_)
                | TyKind::Uint(_)
                | TyKind::Float(_)
                | TyKind::Adt(_, _)
                | TyKind::Foreign(_)
                | TyKind::Str
                | TyKind::Array(_, _)
                | TyKind::Pat(_, _)
                | TyKind::Slice(_)
                | TyKind::FnDef(_, _)
                | TyKind::FnPtr(..)
                | TyKind::Closure(..)
                | TyKind::CoroutineClosure(..)
                | TyKind::Coroutine(_, _)
                | TyKind::Tuple(_)
                | TyKind::UnsafeBinder(_)
        );
        if !source_has_only_impls
            || source.references_error()
            || target.references_error()
            || target.has_aliases()
        {
            return false;
        }
        let interner = self.interner();
        let Some(coerce_unsized) = interner.lang_items().CoerceUnsized else {
            return false;
        };
        // The param env is elaborated, so a bound implying `CoerceUnsized` is among them.
        if self.param_env().clauses.iter().any(|clause| {
            clause.as_trait_clause().is_some_and(|pred| pred.def_id().0 == coerce_unsized)
        }) {
            return false;
        }
        let trait_ref = TraitRef::new(interner, coerce_unsized.into(), [source, target]);
        interner.for_each_relevant_impl(trait_ref, |_| std::ops::ControlFlow::Break(())).is_continue()
    }

    /// The memo's key for the pair, if R3's outcome for it is a function of the two types.
    fn unsize_key(&self, source: Ty<'db>, target: Ty<'db>) -> Option<(Ty<'db>, Ty<'db>)> {
        let memoizable = |ty: Ty<'db>| {
            !ty.has_non_region_infer() && !ty.references_error() && !ty.has_aliases()
        };
        if !memoizable(source) || !memoizable(target) {
            return None;
        }
        let interner = self.interner();
        let erase = |ty: Ty<'db>| {
            if !ty.has_infer_regions() {
                return ty;
            }
            fold_regions(interner, ty, |region, _| match region.kind() {
                RegionKind::ReVar(_) => Region::new_erased(interner),
                _ => region,
            })
        };
        Some((erase(source), erase(target)))
    }
}

#[cfg(test)]
pub(crate) mod tests;
