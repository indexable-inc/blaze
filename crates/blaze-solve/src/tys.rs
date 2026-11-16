//! What the solver asks of the term arena (solve.md, "The seams"): blaze-ty implements it; the
//! tests' arena (`testing::TestTys`) is its model. Every operation is iterative in the arena
//! (no recursion on a term's depth) and allocates only in the arena's own pools.

use blaze_own::{Alloc, Vec};

use crate::error::SolveError;
use crate::term::{Flags, List, Markers, Ty, TyKind, VarKind};

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
///   anything, and where one side is an `Alias` term (not `Opaque` in the solver's analysis
///   mode either: every alias) and the other is not the identical term, the pair is pushed to
///   `deferred` and counted as related: the solver turns each into an `AliasRelate` goal
///   (the reference's lazy normalization, `relate` with `StructurallyRelateAliases::No`).
///   On `Ok(false)` nothing stays bound (the arena rolls back its own partial work) and
///   `deferred` is as it was.
/// - Every binding of a variable (to a term or to another variable) appends the variable's
///   index to the binding log; `rollback_to` truncates the log with the bindings.
/// - Snapshots nest in stack order: `rollback_to` or `commit` of the latest first.
pub trait Tys {
    /// A snapshot of the inference table and the binding log.
    type Snapshot: Copy;

    fn kind(&self, t: Ty) -> TyKind;
    fn flags(&self, t: Ty) -> Flags;
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
    /// Relates `a` and `b` (see the trait's contracts).
    fn unify(&mut self, a: Ty, b: Ty, deferred: &mut Vec<(Ty, Ty), Alloc<'_>>) -> Result<bool, SolveError>;

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
