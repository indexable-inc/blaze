//! What the solver asks of the term arena (solve.md, "The seams"): blaze-ty implements it; the
//! tests' arena (`testing::TestTys`) is its model. Every operation is iterative in the arena
//! (no recursion on a term's depth) and allocates only in the arena's own pools.

use blaze_own::{Alloc, Vec};

use crate::error::SolveError;
use crate::term::{Flags, List, Markers, Ty, TyKind, VarKind};

/// How `Tys::unify` treats an alias.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Relate {
    /// A non-rigid alias (`Rigid::No`) against a term other than itself is deferred: the
    /// solver emits `Projection { alias, term: fresh }` and relates `fresh` with the other side
    /// (rustc_type_ir relate/solver_relating.rs, the `IsRigid::No` arms of `tys`). A rigid
    /// alias relates by structure, a variable binding to it.
    Lazy,
    /// Every alias, rigid or not, relates by structure and nothing is deferred: how a
    /// response's values are applied (rustc_next_trait_solver canonical/mod.rs,
    /// `ResponseRelating` in `unify_query_var_values`: "always structurally relate aliases").
    Structural,
}

/// The arena of terms with its inference table.
///
/// Contracts the solver relies on:
/// - `mk` hash-conses: two equal kinds give one handle, so handle equality is term equality
///   for terms without inference variables.
/// - `kind` is one level, unresolved: a bound variable is still `Infer`; `shallow_resolve`
///   follows bindings at the top.
/// - `unify` is structural: variables bind (with the occurs check; an int variable binds only
///   to an integer type or int variable, a float one likewise), regions always relate without
///   binding (solve.md, "Not ported": region equality is not recorded), `Error` relates to
///   anything, and with `Relate::Lazy`, where one side is a non-rigid alias and the other is
///   not the identical term, the pair (alias first) is pushed to `deferred` and counted as
///   related (see [`Relate`]). On `Ok(false)` nothing stays bound (the arena rolls back its
///   own partial work) and `deferred` is as it was.
/// - `flags` sets `HAS_NON_RIGID_ALIAS` for a term holding an alias with `Rigid::No`.
/// - Every binding of a variable (to a term or to another variable) appends the variable's
///   index to the binding log; `rollback_to` truncates the log with the bindings.
/// - Snapshots nest in stack order: `commit` of the latest first; `rollback_to` of the latest,
///   or of an older one, which closes every snapshot opened after it (the unwind of a stop).
pub trait Tys {
    /// A snapshot of the inference table and the binding log.
    type Snapshot: Copy;

    fn kind(&self, t: Ty) -> TyKind;
    fn flags(&self, t: Ty) -> Flags;
    /// Rebuilds a bound variable at a new De Bruijn index, keeping its original term sort.
    fn reindex_bound(&mut self, _original: Ty, depth: u32, var: u32) -> Result<Ty, SolveError> {
        self.mk(TyKind::Bound(depth, var))
    }
    /// Moves the free bound variables of an argument under `by` new binders.
    /// An arena with separate term sorts can preserve the sort using its own shifter.
    fn shift_in(&mut self, scratch: &mut crate::fold::Scratch<'_>, t: Ty, by: u32) -> Result<Ty, SolveError>
    where
        Self: Sized,
    {
        crate::fold::fold_at_depth(self, scratch, t, |tys, _, x, depth| {
            if !tys.flags(x).has(Flags::HAS_BOUND) {
                return Ok(Some(x));
            }
            match tys.kind(x) {
                TyKind::Bound(d, i) if d >= depth => {
                    let d = d.checked_add(by).ok_or(SolveError::Limit(crate::error::Limit::Walk))?;
                    Ok(Some(tys.reindex_bound(x, d, i)?))
                }
                _ => Ok(None),
            }
        })
    }
    /// The marker answers kept on `t` (solve.md (c)).
    fn markers(&self, t: Ty) -> Markers;
    fn set_markers(&mut self, t: Ty, m: Markers);
    /// The `i`th term of `l`; `i < l.len`.
    fn list_get(&self, l: List, i: u32) -> Ty;

    fn mk(&mut self, k: TyKind) -> Result<Ty, SolveError>;
    fn mk_list(&mut self, items: &[Ty]) -> Result<List, SolveError>;
    /// A new unbound variable of `kind`, as a term.
    fn fresh(&mut self, kind: VarKind) -> Result<Ty, SolveError>;
    /// `t` with a bound variable at its top replaced by its value, repeatedly.
    fn shallow_resolve(&self, t: Ty) -> Ty;
    /// Relates `a` and `b` (see the trait's contracts). With `Relate::Structural` an alias is
    /// related to another term by its structure like any other term, and nothing is deferred
    /// (the reference's `StructurallyRelateAliases::Yes`, used to apply a response).
    fn unify(&mut self, a: Ty, b: Ty, how: Relate, deferred: &mut Vec<(Ty, Ty), Alloc<'_>>) -> Result<bool, SolveError>;

    fn snapshot(&mut self) -> Self::Snapshot;
    fn rollback_to(&mut self, s: Self::Snapshot);
    fn commit(&mut self, s: Self::Snapshot);

    /// The binding log's length.
    fn bindings_len(&self) -> u32;
    /// The variable index bound at position `i` of the binding log.
    fn binding(&self, i: u32) -> u32;
    /// One past the largest variable index made so far.
    fn var_count(&self) -> u32;
}
