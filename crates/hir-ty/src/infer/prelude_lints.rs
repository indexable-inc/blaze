//! Which method calls and type-relative paths a later edition resolves to another item: rustc's
//! `compiler/rustc_hir_typeck/src/method/prelude_edition_lints.rs` (at 16d1ffac). What its
//! messages and suggestions are written from is recorded; the diagnostics write them.

use base_db::{Crate, Db};
use hir_def::{
    AdtId, AttrDefId, HasModule,
    diagnostic_item::diagnostic_items,
    hir::{Expr, ExprId, ExprOrPatIdPacked},
    signatures::FunctionSignature,
};
use hir_expand::name::Name;
use intern::sym;
use rustc_ast_ir::Mutability;
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind as _};
use span::Edition;

use crate::{
    InferenceDiagnostic,
    infer::InferenceContext,
    method_resolution::{AutorefOrPtrAdjustment, CandidateId, Pick, PickKind},
    next_solver::{Ty, TyKind},
};

/// rustc's `prelude_or_array_lint`: the lint of a call a later edition resolves otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EditionLint {
    /// `rust_2021_prelude_collisions`: `try_into`, and the paths `try_from`, `try_into` and
    /// `from_iter`.
    Rust2021PreludeCollisions,
    /// `rust_2024_prelude_collisions`: `poll` on a `Pin<&mut _>`, and `into_future`.
    Rust2024PreludeCollisions,
    /// `array_into_iter`: `into_iter` on an array.
    ArrayIntoIter,
    /// `boxed_slice_into_iter`: `into_iter` on a boxed slice.
    BoxedSliceIntoIter,
}

impl EditionLint {
    /// The edition that resolves the call otherwise.
    pub fn edition(self) -> Edition {
        match self {
            EditionLint::Rust2021PreludeCollisions | EditionLint::ArrayIntoIter => {
                Edition::Edition2021
            }
            EditionLint::Rust2024PreludeCollisions | EditionLint::BoxedSliceIntoIter => {
                Edition::Edition2024
            }
        }
    }
}

/// rustc's `AutorefOrPtrAdjustment` of a picked method's receiver, as the suggestions write it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReceiverAdjustment {
    /// `&` or `&mut`.
    Autoref(Mutability),
    /// `as *const _`.
    ToConstPtr,
}

/// rustc's `STDLIB_STABLE_CRATES`: a method of one of them will be in the new prelude.
fn is_stdlib_stable_crate(db: &Db, krate: Crate) -> bool {
    krate.extra_data(db).display_name.as_ref().is_some_and(|it| {
        let name = it.crate_name().symbol();
        *name == sym::std || *name == sym::core || *name == sym::alloc || *name == sym::proc_macro
    })
}

impl<'db, 'f> InferenceContext<'db, 'f> {
    /// rustc's `lint_edition_dependent_dot_call`: the method call `call_expr` on `receiver` of
    /// type `self_ty`, picked as `pick`.
    pub(crate) fn lint_edition_dependent_dot_call(
        &mut self,
        self_ty: Ty<'db>,
        call_expr: ExprId,
        receiver: ExprId,
        pick: &Pick<'db>,
    ) {
        // A field access with no field is looked up as a method too, which rustc does not lint.
        if !matches!(self.store.expr(call_expr), Expr::MethodCall { .. }) {
            return;
        }
        let CandidateId::FunctionId(method) = pick.item else {
            unreachable!("a method call picks a function")
        };
        let edition = self.edition;
        let signature = FunctionSignature::of(self.db, method);
        let name = signature.name.symbol();
        let lint = if *name == sym::try_into && !edition.at_least_2021() {
            EditionLint::Rust2021PreludeCollisions
        } else if *name == sym::poll && !edition.at_least_2024() && self.is_pin_of_mut_ref(self_ty)
        {
            EditionLint::Rust2024PreludeCollisions
        } else if *name == sym::into_future && !edition.at_least_2024() {
            EditionLint::Rust2024PreludeCollisions
        } else if *name == sym::into_iter {
            if matches!(self_ty.kind(), TyKind::Array(..)) && !edition.at_least_2021() {
                EditionLint::ArrayIntoIter
            } else if self_ty.boxed_ty().is_some_and(Ty::is_slice) && !edition.at_least_2024() {
                EditionLint::BoxedSliceIntoIter
            } else {
                return;
            }
        } else {
            return;
        };

        if is_stdlib_stable_crate(self.db, method.krate(self.db)) {
            return;
        }

        let autoref = pick.autoref_or_ptr_adjustment.map(|it| match it {
            AutorefOrPtrAdjustment::Autoref { mutbl, .. } => ReceiverAdjustment::Autoref(mutbl),
            AutorefOrPtrAdjustment::ToConstPtr => ReceiverAdjustment::ToConstPtr,
        });
        let inherent = matches!(pick.kind, PickKind::InherentImplPick(_) | PickKind::ObjectPick(_));
        if inherent {
            // Avoid repeatedly adding unneeded `&*`s.
            if pick.autoderefs == 1
                && matches!(autoref, Some(ReceiverAdjustment::Autoref(_)))
                && matches!(self_ty.kind(), TyKind::Ref(..))
            {
                return;
            }
            // An inherent `self` method takes precedence over the trait's.
            if pick.autoderefs == 0 && autoref.is_none() {
                return;
            }
        }

        self.push_diagnostic(InferenceDiagnostic::EditionDependentMethodCall {
            call_expr,
            receiver,
            lint,
            method,
            inherent,
            autoderefs: u32::try_from(pick.autoderefs).expect("2^32 autoderefs"),
            autoref,
        });
    }

    /// rustc's `lint_fully_qualified_call_from_2018`: the type-relative path `node` naming
    /// `name` of `self_ty`, picked as `pick`.
    pub(crate) fn lint_fully_qualified_call_from_2018(
        &mut self,
        node: ExprOrPatIdPacked,
        name: &Name,
        self_ty: Ty<'db>,
        pick: &Pick<'db>,
    ) {
        if self.edition.at_least_2021() {
            return;
        }
        let from_iter = *name.symbol() == sym::from_iter;
        if !(*name.symbol() == sym::try_into || *name.symbol() == sym::try_from || from_iter) {
            return;
        }
        let krate = match pick.item {
            CandidateId::FunctionId(it) => it.krate(self.db),
            CandidateId::ConstId(it) => it.krate(self.db),
        };
        if is_stdlib_stable_crate(self.db, krate) {
            return;
        }
        // Not for a type known not to implement `FromIterator`.
        if from_iter
            && let Some(AttrDefId::TraitId(from_iterator)) =
                diagnostic_items(self.db, self.krate()).get(&sym::FromIterator)
        {
            let any_type = self.table.next_ty_var(node.into());
            if !self
                .table
                .infer_ctxt
                .type_implements_trait(from_iterator, [self_ty, any_type], self.table.param_env)
                .may_apply()
            {
                return;
            }
        }
        // An inherent item of the type takes precedence over the trait's.
        if matches!(pick.kind, PickKind::InherentImplPick(_)) {
            return;
        }

        self.push_diagnostic(InferenceDiagnostic::FullyQualifiedCallFrom2018 {
            node,
            item: pick.item,
            self_ty: self_ty.store(),
        });
    }

    fn is_pin_of_mut_ref(&self, ty: Ty<'db>) -> bool {
        let Some(pin) = self.lang_items.Pin else { return false };
        match ty.as_adt() {
            Some((AdtId::StructId(it), args)) if it == pin => {
                matches!(args.type_at(0).kind(), TyKind::Ref(_, _, Mutability::Mut))
            }
            _ => false,
        }
    }
}
