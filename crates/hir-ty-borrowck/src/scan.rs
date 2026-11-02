//! What the checks read of the owner's expression store, found in one pass over it. The store
//! is the owner's, which its closures, coroutines and `const { .. }` blocks share, so each node
//! is decoded once for all of them, not once per table and again per body.

use hir_def::{
    expr_store::ExpressionStore,
    hir::{BindingId, Expr, ExprId, Pat, PatId, Statement},
};
use hir_ty::{
    InferenceResult,
    next_solver::{DbInterner, Region, RegionExt as _, Ty},
};
use crate::{
    bitset::{BitSet, bits},
    mem::{self, FMap, FVec, Frame},
    regions::{Annotation, spells_generics},
};

#[cfg(test)]
pub(crate) mod tests;

/// In [`Scan::tested_at`], a pattern no `match` arm has; in a parent map, a root.
const NONE: u32 = u32::MAX;

pub(crate) struct Scan<'f, 'db> {
    /// The `match` expressions, and what they match, whose arms test the value: rustc reads the
    /// matched place on the edge taken when no arm matches, which a match of bindings alone
    /// lacks. By expression.
    matched: BitSet<'f>,
    /// By pattern, the scrutinee of the `match` an arm's pattern is in, [`NONE`] for none:
    /// rustc reads a discriminant and switches on a value at the scrutinee's span, where
    /// rust-analyzer gives the pattern's. Empty when the store has no `match`.
    tested_at: FVec<'f, u32>,
    /// Record literals and the values of their fields, whose lowering assigns a field of the
    /// literal where rustc assigns the whole of it (it matters for a union). By expression.
    union_literals: BitSet<'f>,
    pub(crate) guarded: GuardedMatches<'f>,
    /// The expression a variable's pattern matches on, for each variable a `match`, a `let`
    /// with a pattern or an `if let` binds; `None` for a `let` of the variable alone.
    scrutinees: FMap<'f, BindingId, Option<ExprId>>,
    /// For each expression in a `match` arm's guard, the variables the arm's pattern binds:
    /// rustc binds them by reference in the guard, so it cannot move out of them.
    guard_bindings: FMap<'f, ExprId, FVec<'f, BindingId>>,
    /// The values of expression statements (`match x { None => &temp(), Some(it) => it };`),
    /// which lowering reads once the statement's temporaries are dropped, and rustc does not;
    /// with, for a `match`, the type of its value, its regions erased, which tells that read
    /// from the read of what it matches (they have the same span), where the two types differ.
    statement_values: FMap<'f, ExprId, Option<Ty<'db>>>,
    /// The `let`s with a type written out.
    pub(crate) lets: FVec<'f, Annotation>,
    /// The whole patterns of a `let` without a type and of a `match` arm that are a struct,
    /// variant or constant path spelling out generic arguments, with what the `let` reads or
    /// the `match` matches.
    pub(crate) spelled_patterns: FVec<'f, (PatId, Option<ExprId>)>,
    /// The path expressions that spell out generic arguments, with the closure or `async`
    /// block each is directly in, `None` for the owner's body.
    pub(crate) spelled_paths: FVec<'f, (ExprId, Option<ExprId>)>,
}

/// The `match`es with a guard in some arm, numbered in the store's order, as the guards' checks
/// need them of every body: which `match` each pattern of their arms is in, and their guards.
pub(crate) struct GuardedMatches<'f> {
    /// How many there are.
    pub(crate) matches: usize,
    /// The `match` each pattern of an arm is in.
    pub(crate) tested_by: FMap<'f, PatId, usize>,
    /// For each binding pattern of an arm, its `match` and its arm's guard, if it has one.
    pub(crate) bound_by: FMap<'f, PatId, (usize, Option<usize>)>,
    /// Each guard, numbered by its place here, with its `match`; a `match`'s in the order of
    /// its arms, and the `match`es in order.
    pub(crate) guards: FVec<'f, (usize, ExprId)>,
}

#[inline]
fn raw<T>(id: la_arena::Idx<T>) -> usize {
    id.into_raw().into_u32() as usize
}

#[inline]
fn expr_at(raw: u32) -> ExprId {
    ExprId::from_raw(la_arena::RawIdx::from_u32(raw))
}

/// Whether the whole pattern `pat` is a path that spells out generic arguments.
fn spelled(pat: Pat<'_>) -> bool {
    match pat {
        Pat::Record { path, .. } | Pat::TupleStruct { path, .. } | Pat::Path(path) => {
            spells_generics(path)
        }
        _ => false,
    }
}

impl<'f, 'db> Scan<'f, 'db> {
    pub(crate) fn new(
        frame: &'f Frame,
        store: &ExpressionStore,
        infer: &InferenceResult<'db>,
        interner: DbInterner<'db>,
    ) -> Self {
        let erased = |ty: Ty<'db>| {
            rustc_type_ir::fold_regions(interner, ty, |_, _| Region::new_erased(interner))
        };
        let count = store.expr_count();
        let mut matched = bits(frame, count);
        let mut union_literals = bits(frame, count);
        let mut tested_at = Vec::new_in(frame);
        let mut guarded = GuardedMatches {
            matches: 0,
            tested_by: mem::map(frame),
            bound_by: mem::map(frame),
            guards: Vec::new_in(frame),
        };
        let mut scrutinees = mem::map(frame);
        let mut guard_bindings: FMap<'f, ExprId, FVec<'f, BindingId>> = mem::map(frame);
        let mut statement_values = mem::map(frame);
        let (mut lets, mut spelled_patterns, mut spelled_paths) =
            (Vec::new_in(frame), Vec::new_in(frame), Vec::new_in(frame));
        let mut bindings: FVec<'_, BindingId> = Vec::new_in(frame);
        let mut stack: FVec<'_, ExprId> = Vec::new_in(frame);
        for (id, expr) in store.exprs() {
            match expr {
                Expr::Match { expr: scrutinee, arms } => {
                    if arms.is_empty()
                        || arms.iter().any(|arm| !irrefutable(store, infer, arm.pat))
                    {
                        matched.insert(raw(id));
                        matched.insert(raw(scrutinee));
                    }
                    if tested_at.is_empty() {
                        tested_at.resize(store.pat_count(), NONE);
                    }
                    let has_guard = arms.iter().any(|arm| arm.guard.is_some());
                    let index = guarded.matches;
                    for arm in arms {
                        if spelled(store.pat(arm.pat)) {
                            spelled_patterns.push((arm.pat, Some(scrutinee)));
                        }
                        let guard = match (has_guard, arm.guard) {
                            (true, Some(guard)) => {
                                guarded.guards.push((index, guard));
                                Some(guarded.guards.len() - 1)
                            }
                            _ => None,
                        };
                        bindings.clear();
                        store.walk_pats(arm.pat, &mut |pat| {
                            tested_at[raw(pat)] = raw(scrutinee) as u32;
                            if has_guard {
                                guarded.tested_by.insert(pat, index);
                            }
                            if let Pat::Bind { id: binding, .. } = store.pat(pat) {
                                if has_guard {
                                    guarded.bound_by.insert(pat, (index, guard));
                                }
                                scrutinees.insert(binding, Some(scrutinee));
                                bindings.push(binding);
                            }
                        });
                        if let Some(guard) = arm.guard
                            && !bindings.is_empty()
                        {
                            stack.push(guard);
                            while let Some(expr) = stack.pop() {
                                guard_bindings
                                    .entry(expr)
                                    .or_insert_with(|| Vec::new_in(frame))
                                    .extend_from_slice(&bindings);
                                store.walk_child_exprs(expr, |it| stack.push(it));
                            }
                        }
                    }
                    if has_guard {
                        guarded.matches += 1;
                    }
                }
                Expr::Let { pat, expr: value } => {
                    store.walk_bindings_in_pat(pat, |binding| {
                        scrutinees.insert(binding, Some(value));
                    });
                }
                Expr::Block { statements, .. } => {
                    for statement in statements.iter() {
                        match statement {
                            Statement::Let { pat, type_ref, initializer, .. } => {
                                let whole = store.pat(pat);
                                if let Some(init) = initializer {
                                    let simple =
                                        matches!(whole, Pat::Bind { subpat: None, .. });
                                    let scrutinee = if simple { None } else { Some(init) };
                                    store.walk_bindings_in_pat(pat, |binding| {
                                        scrutinees.insert(binding, scrutinee);
                                    });
                                }
                                match type_ref {
                                    Some(type_ref) => lets.push(Annotation {
                                        pat,
                                        type_ref,
                                        initializer,
                                        block: id,
                                    }),
                                    None if spelled(whole) => {
                                        spelled_patterns.push((pat, initializer))
                                    }
                                    None => (),
                                }
                            }
                            Statement::Expr { expr, .. } => match store.expr(expr) {
                                Expr::Match { expr: of, .. } => {
                                    let value = erased(infer.expr_ty(expr));
                                    if value != erased(infer.expr_ty(of)) {
                                        statement_values.insert(expr, Some(value));
                                    }
                                }
                                _ => {
                                    statement_values.insert(expr, None);
                                }
                            },
                            _ => (),
                        }
                    }
                }
                Expr::RecordLit { fields, .. } => {
                    union_literals.insert(raw(id));
                    for field in fields.iter() {
                        union_literals.insert(raw(field.expr));
                    }
                }
                Expr::Path(path) if spells_generics(path) => spelled_paths.push((id, None)),
                _ => (),
            }
        }
        if !spelled_paths.is_empty() {
            let parent = frame.alloc_slice_fill_with(count, |_| NONE);
            for expr in store.expr_ids() {
                store.walk_child_exprs(expr, |child| parent[raw(child)] = raw(expr) as u32);
            }
            for (path, closure) in &mut spelled_paths {
                let mut at = raw(*path);
                *closure = loop {
                    let up = parent[at];
                    if up == NONE {
                        break None;
                    }
                    if matches!(store.expr(expr_at(up)), Expr::Closure { .. }) {
                        break Some(expr_at(up));
                    }
                    at = up as usize;
                };
            }
        }
        Scan {
            matched,
            tested_at,
            union_literals,
            guarded,
            scrutinees,
            guard_bindings,
            statement_values,
            lets,
            spelled_patterns,
            spelled_paths,
        }
    }

    pub(crate) fn is_matched(&self, expr: ExprId) -> bool {
        self.matched.contains(raw(expr))
    }

    pub(crate) fn tested_at(&self, pat: PatId) -> Option<ExprId> {
        let &at = self.tested_at.get(raw(pat))?;
        (at != NONE).then(|| expr_at(at))
    }

    pub(crate) fn is_union_literal(&self, expr: ExprId) -> bool {
        self.union_literals.contains(raw(expr))
    }

    pub(crate) fn scrutinee_of(&self, binding: BindingId) -> Option<Option<ExprId>> {
        self.scrutinees.get(&binding).copied()
    }

    pub(crate) fn guard_bindings(&self, expr: ExprId) -> &[BindingId] {
        self.guard_bindings.get(&expr).map_or(&[], |it| it.as_slice())
    }

    pub(crate) fn statement_value(&self, expr: ExprId) -> Option<Option<Ty<'db>>> {
        self.statement_values.get(&expr).copied()
    }
}

/// Whether `pat` matches every value of its type, with no test of it.
fn irrefutable(store: &ExpressionStore, infer: &InferenceResult<'_>, pat: PatId) -> bool {
    let struct_like = || {
        matches!(
            infer.variant_resolution_for_pat(pat),
            Some(hir_def::VariantId::StructId(_) | hir_def::VariantId::UnionId(_))
        )
    };
    match store.pat(pat) {
        Pat::Wild | Pat::Rest | Pat::Bind { subpat: None, .. } => true,
        Pat::Bind { subpat: Some(inner), .. }
        | Pat::Ref { pat: inner, .. }
        | Pat::Box { inner }
        | Pat::Deref { inner } => irrefutable(store, infer, inner),
        Pat::Tuple { args, .. } => args.iter().all(|&it| irrefutable(store, infer, it)),
        Pat::TupleStruct { args, .. } => {
            struct_like() && args.iter().all(|&it| irrefutable(store, infer, it))
        }
        Pat::Record { args, .. } => {
            struct_like() && args.iter().all(|it| irrefutable(store, infer, it.pat))
        }
        _ => false,
    }
}
