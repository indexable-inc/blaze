//! blaze: flow narrowing of `#![blaze::pattern_types]` (docs/v2/plan/design/edition.md,
//! section 8). In a match arm, the success branch of an `if let` and after a `let else`, a
//! scrutinee that is a plain local, or a shared borrow of one, has the type narrowed to the
//! variants of its enum the pattern admits (`Conn is Conn::Open { .. }`), so a use of the local
//! there can be passed where the narrowed type is asked for, with no second match.
//!
//! Only a local that is neither reassigned nor uniquely borrowed is narrowed: one bound without
//! `mut` (`BindingAnnotation::Unannotated` or `Ref`), which nothing can assign or borrow
//! uniquely once it holds a value. A local of a `&mut` type is not narrowed. Nothing else is:
//! no field, no index, no deref of a box.

use hir_def::{
    EnumId, Lookup, VariantId,
    hir::{BinaryOp, BindingAnnotation, BindingId, Expr, ExprId, LogicOp, Pat, PatId, UnaryOp},
    resolver::ValueNs,
    type_ref::{Mutability as HirMutability, Rawness},
};
use rustc_ast_ir::Mutability;
use rustc_type_ir::inherent::{IntoKind, Ty as _};
use smallvec::SmallVec;

use crate::{
    next_solver::{Ty, TyKind},
    pattern_types,
};

use super::{InferenceContext, InferenceDiagnostic};

impl<'db, 'f> InferenceContext<'db, 'f> {
    /// The type `binding` is narrowed to where inference is now, innermost first.
    pub(super) fn narrowed_type(&self, binding: BindingId) -> Option<Ty<'db>> {
        self.narrowed.iter().rev().find(|(it, _)| *it == binding).map(|&(_, ty)| ty)
    }

    /// How many narrowings hold now, to drop the ones made after with [`Self::unnarrow`].
    pub(super) fn narrowed_mark(&self) -> usize {
        self.narrowed.len()
    }

    /// Drops the narrowings made since `mark`: the arm, branch or block they held in ended.
    pub(super) fn unnarrow(&mut self, mark: usize) {
        self.narrowed.truncate(mark);
    }

    /// Narrows the local `scrutinee` reads to the variants `pat` admits, once `pat` is inferred:
    /// for a match arm, an `if let`'s success branch or what follows a `let else`.
    pub(super) fn narrow_by_pattern(&mut self, scrutinee: ExprId, pat: PatId) {
        if !self.pattern_types {
            return;
        }
        if let Some(narrowing) = self.narrowing(scrutinee, pat) {
            self.narrowed.push(narrowing);
        }
    }

    /// Narrows by each `let` of an `if`'s condition, through `&&` (a let chain), for its
    /// success branch.
    pub(super) fn narrow_by_condition(&mut self, condition: ExprId) {
        if !self.pattern_types {
            return;
        }
        match self.store.expr(condition) {
            Expr::Let { pat, expr } => self.narrow_by_pattern(expr, pat),
            Expr::BinaryOp { lhs, rhs, op: Some(BinaryOp::LogicOp(LogicOp::And)) } => {
                self.narrow_by_condition(lhs);
                self.narrow_by_condition(rhs);
            }
            _ => {}
        }
    }

    /// Rule pattern-types-unique-ref: `*r = ..` with `r: &mut (T is PAT)` writes the whole
    /// value through a unique reference to a refinement, which is refused, naming the place.
    /// A write to a field of it (`r.field = ..`) is a place of the field's type and passes.
    /// A `&mut T` is never made from a `&mut (T is PAT)`: `&mut` is invariant, so a refinement
    /// widens to its base only under covariance (`SolverRelating::tys`).
    pub(super) fn check_refinement_write(&mut self, place: ExprId) {
        if !self.pattern_types {
            return;
        }
        let Expr::UnaryOp { expr: operand, op: UnaryOp::Deref } = self.store.expr(place) else {
            return;
        };
        let operand_ty = self.expr_ty_after_adjustments(operand);
        let operand_ty = self.table.try_structurally_resolve_type(operand.into(), operand_ty);
        let TyKind::Ref(_, pointee, Mutability::Mut) = operand_ty.kind() else { return };
        let pointee = self.table.try_structurally_resolve_type(place.into(), pointee);
        if pattern_types::refinement(pointee).is_some() {
            self.push_diagnostic(InferenceDiagnostic::WholeValueWriteThroughRefinement {
                place,
                ty: pointee.store(),
            });
        }
    }

    /// The local `scrutinee` reads and its type narrowed by `pat`, when it is one to narrow.
    fn narrowing(&mut self, scrutinee: ExprId, pat: PatId) -> Option<(BindingId, Ty<'db>)> {
        let (place, borrowed) = match self.store.expr(scrutinee) {
            Expr::Ref { expr, rawness: Rawness::Ref, mutability: HirMutability::Shared } => {
                (expr, true)
            }
            _ => (scrutinee, false),
        };
        let Expr::Path(path) = self.store.expr(place) else { return None };
        let guard = self.resolver.update_to_inner_scope(self.db, self.store_owner, place);
        let resolved = self.resolver.resolve_path_in_value_ns_fully(
            self.db,
            path,
            self.store.expr_path_hygiene(place),
        );
        self.resolver.reset_to_guard(guard);
        let Some(ValueNs::LocalBinding(binding)) = resolved else { return None };
        if matches!(
            self.store[binding].mode,
            BindingAnnotation::Mutable | BindingAnnotation::RefMut
        ) {
            return None;
        }
        let current = match self.narrowed_type(binding) {
            Some(ty) => ty,
            None => self.result.type_of_binding.get(binding)?.as_ref(),
        };
        let current = self.table.try_structurally_resolve_type(place.into(), current);
        let (region, enum_ty) = match current.kind() {
            TyKind::Ref(region, inner, Mutability::Not) if !borrowed => {
                (Some(region), self.table.try_structurally_resolve_type(place.into(), inner))
            }
            TyKind::Ref(..) => return None,
            _ => (None, current),
        };
        let enum_ = pattern_types::enum_of(enum_ty)?;
        let mut admitted = self.pattern_variants(pat, enum_)?;
        if let Some((_, pattern)) = pattern_types::refinement(enum_ty) {
            let held = pattern_types::variants(pattern);
            admitted.retain(|it| held.contains(it));
        }
        let variant_count = enum_.enum_variants(self.db).variants.len();
        let interner = self.interner();
        let refined =
            pattern_types::refine(interner, pattern_types::base(enum_ty), admitted, variant_count)?;
        let ty = match region {
            Some(region) => Ty::new_ref(interner, region, refined, Mutability::Not),
            None => refined,
        };
        Some((binding, ty))
    }

    /// The variants of `enum_` a pattern admits at its top, or `None` when it admits any (a
    /// binding, `_`) or is not a pattern of the enum.
    fn pattern_variants(&self, pat: PatId, enum_: EnumId) -> Option<pattern_types::Variants> {
        match self.store.pat(pat) {
            Pat::Path(_) | Pat::TupleStruct { .. } | Pat::Record { .. } => {
                match self.result.variant_resolution_for_pat(pat)? {
                    VariantId::EnumVariantId(variant)
                        if variant.lookup(self.db).parent == enum_ =>
                    {
                        Some(SmallVec::from_slice(&[variant.index(self.db) as u32]))
                    }
                    _ => None,
                }
            }
            Pat::Or(pats) => {
                let mut all = pattern_types::Variants::new();
                for &it in pats {
                    all.extend(self.pattern_variants(it, enum_)?);
                }
                Some(all)
            }
            Pat::Bind { subpat: Some(subpat), .. } => self.pattern_variants(subpat, enum_),
            Pat::Ref { pat, mutability: HirMutability::Shared } => {
                self.pattern_variants(pat, enum_)
            }
            _ => None,
        }
    }
}
