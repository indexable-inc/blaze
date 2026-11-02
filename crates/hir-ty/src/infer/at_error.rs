//! Types as inference has them when it finds an error, which rustc prints: the type of an
//! integer or float literal not decided yet is `{integer}` or `{float}` there, where after
//! fallback it is `i32` or `f64`.

use rustc_type_ir::{
    InferTy, TypeFoldable, TypeFolder, TypeSuperFoldable, TypeVisitableExt,
    inherent::{IntoKind, Ty as _},
};

use crate::{
    infer::InferenceContext,
    next_solver::{Const, DbInterner, ErrorGuaranteed, Region, RegionExt, StoredTy, Ty, TyKind},
};

impl<'db, 'f> InferenceContext<'db, 'f> {
    /// `expected` and `found` with what is known of them now, the types of undecided numeric
    /// literals as `FreshIntTy` / `FreshFloatTy` and any other variable an error; `None` when
    /// neither holds such a literal's type, so writeback's resolved types say the same.
    pub(super) fn types_at_error(
        &self,
        expected: Ty<'db>,
        found: Ty<'db>,
    ) -> Option<Box<(StoredTy, StoredTy)>> {
        let expected = self.table.resolve_vars_if_possible(expected);
        let found = self.table.resolve_vars_if_possible(found);
        if !expected.has_infer_types() && !found.has_infer_types() {
            return None;
        }
        let mut folder = NumericVarsAtError { interner: self.interner(), numeric: false };
        let expected = expected.fold_with(&mut folder);
        let found = found.fold_with(&mut folder);
        // alloc: error path, one per type mismatch of a literal
        folder.numeric.then(|| Box::new((expected.store(), found.store())))
    }

    /// `ty` as [`Self::types_at_error`] gives it, whether or not it holds a literal's type.
    pub(super) fn type_at_error(&self, ty: Ty<'db>) -> StoredTy {
        let ty = self.table.resolve_vars_if_possible(ty);
        if !ty.has_infer() {
            return ty.store();
        }
        ty.fold_with(&mut NumericVarsAtError { interner: self.interner(), numeric: false }).store()
    }
}

struct NumericVarsAtError<'db> {
    interner: DbInterner<'db>,
    /// Whether a numeric variable was found.
    numeric: bool,
}

impl<'db> TypeFolder<DbInterner<'db>> for NumericVarsAtError<'db> {
    fn cx(&self) -> DbInterner<'db> {
        self.interner
    }

    fn fold_ty(&mut self, ty: Ty<'db>) -> Ty<'db> {
        if !ty.has_infer() {
            return ty;
        }
        match ty.kind() {
            TyKind::Infer(InferTy::IntVar(_)) => {
                self.numeric = true;
                Ty::new_fresh_int(self.interner, 0)
            }
            TyKind::Infer(InferTy::FloatVar(_)) => {
                self.numeric = true;
                Ty::new_fresh_float(self.interner, 0)
            }
            TyKind::Infer(_) => Ty::new_error(self.interner, ErrorGuaranteed),
            _ => ty.super_fold_with(self),
        }
    }

    fn fold_const(&mut self, ct: Const<'db>) -> Const<'db> {
        if !ct.has_infer() {
            return ct;
        }
        if ct.is_ct_infer() { Const::new_error(self.interner, ErrorGuaranteed) } else { ct.super_fold_with(self) }
    }

    fn fold_region(&mut self, region: Region<'db>) -> Region<'db> {
        if region.is_var() { Region::error(self.interner) } else { region }
    }
}
