//! An undecided term with the variables made for generic parameters kept apart, for rustc's
//! `ty_infer_name_resolver` ("consider giving `x` an explicit type: `Vec<T>`").

use hir_def::{
    GenericParamId,
    hir::generics::{GenericParams, TypeParamProvenance},
};
use rustc_type_ir::{
    InferConst, InferTy, TypeFlags, TypeFoldable as _, TypeFolder, TypeSuperFoldable,
    TypeVisitableExt, inherent::{IntoKind, Ty as _}, solve::TyOrConstInferVar,
};

use crate::{
    Span,
    next_solver::{
        Const, ConstKind, DbInterner, ErrorGuaranteed, GenericArg, ParamConst, Region, RegionExt,
        StoredGenericArg, Term, Ty, TyKind, infer::InferCtxt,
    },
};

/// An undecided term as rustc's `ty_infer_name_resolver` prints it. In `term`, each inference
/// variable made for a type or const parameter is `FreshTy(i)` (a constant `InferConst::Fresh(i)`)
/// with `params[i]` where it was made and the parameter, and every other variable is an error.
/// rustc names no trait's `Self`, no `impl Trait` argument's parameter, and no variable made in
/// a macro expansion; the last is the caller's to decide in [`NamedTerm::named`].
///
/// Not `TypeFoldable`: writeback's resolution would make its fresh variables errors.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NamedTerm {
    pub term: StoredGenericArg,
    pub params: Vec<(Span, GenericParamId)>,
}

impl NamedTerm {
    pub(crate) fn new<'db>(infcx: &InferCtxt<'db>, term: Term<'db>) -> Self {
        // alloc: error path, the variables of a term a diagnostic names
        let mut namer = Namer { infcx, vars: Vec::new(), params: Vec::new() };
        let term = GenericArg::from(term).fold_with(&mut namer).store();
        NamedTerm { term, params: namer.params }
    }

    /// `term` with `FreshTy(i)` and `InferConst::Fresh(i)` the parameter of `params[i]` where
    /// `name(params[i])`, and an error elsewhere.
    pub fn named<'db>(
        &'db self,
        interner: DbInterner<'db>,
        mut name: impl FnMut(Span, GenericParamId) -> bool,
    ) -> GenericArg<'db> {
        let params = self.params.iter().map(|&(span, param)| name(span, param).then_some(param));
        // alloc: error path, one per named variable of a reported diagnostic
        let params = params.collect();
        self.term.as_ref().fold_with(&mut Unnamer { interner, params })
    }
}

/// The parameter a variable was made for, when rustc names it: a type parameter other than a
/// trait's `Self` or a const parameter.
pub(crate) fn variable_param<'db>(
    infcx: &InferCtxt<'db>,
    var: TyOrConstInferVar,
) -> Option<(Span, GenericParamId)> {
    let (span, param) = match var {
        TyOrConstInferVar::Ty(vid) => infcx.type_var_origin(vid),
        TyOrConstInferVar::Const(vid) => infcx.const_var_origin(vid)?,
        TyOrConstInferVar::TyInt(_) | TyOrConstInferVar::TyFloat(_) => return None,
    };
    let param = param?;
    if let GenericParamId::TypeParamId(it) = param
        && GenericParams::of(infcx.interner.db, it.parent())[it.local_id()].is_trait_self()
    {
        return None;
    }
    Some((span, param))
}

struct Namer<'a, 'db> {
    infcx: &'a InferCtxt<'db>,
    vars: Vec<TyOrConstInferVar>,
    params: Vec<(Span, GenericParamId)>,
}

impl Namer<'_, '_> {
    /// The index of `var` among the named variables, a variable met twice named once.
    fn index(&mut self, var: TyOrConstInferVar) -> Option<u32> {
        if let Some(index) = self.vars.iter().position(|&it| it == var) {
            return Some(index as u32);
        }
        let (span, param) = variable_param(self.infcx, var)?;
        if let GenericParamId::TypeParamId(it) = param
            && GenericParams::of(self.infcx.interner.db, it.parent())[it.local_id()]
                .type_param()
                .is_some_and(|it| it.provenance == TypeParamProvenance::ArgumentImplTrait)
        {
            return None;
        }
        self.vars.push(var); // alloc: error path, one per named variable
        self.params.push((span, param));
        Some(self.params.len() as u32 - 1)
    }
}

impl<'db> TypeFolder<DbInterner<'db>> for Namer<'_, 'db> {
    fn cx(&self) -> DbInterner<'db> {
        self.infcx.interner
    }

    fn fold_ty(&mut self, t: Ty<'db>) -> Ty<'db> {
        if !t.has_infer() {
            return t;
        }
        match t.kind() {
            TyKind::Infer(InferTy::TyVar(vid)) => match self.index(TyOrConstInferVar::Ty(vid)) {
                Some(index) => Ty::new_infer(self.cx(), InferTy::FreshTy(index)),
                None => Ty::new_error(self.cx(), ErrorGuaranteed),
            },
            TyKind::Infer(_) => Ty::new_error(self.cx(), ErrorGuaranteed),
            _ => t.super_fold_with(self),
        }
    }

    fn fold_const(&mut self, c: Const<'db>) -> Const<'db> {
        if !c.has_infer() {
            return c;
        }
        match c.kind() {
            ConstKind::Infer(InferConst::Var(vid)) => {
                match self.index(TyOrConstInferVar::Const(vid)) {
                    Some(index) => Const::new_infer(self.cx(), InferConst::Fresh(index)),
                    None => Const::new_error(self.cx(), ErrorGuaranteed),
                }
            }
            ConstKind::Infer(_) => Const::new_error(self.cx(), ErrorGuaranteed),
            _ => c.super_fold_with(self),
        }
    }

    fn fold_region(&mut self, r: Region<'db>) -> Region<'db> {
        if r.is_var() { Region::error(self.cx()) } else { r }
    }
}

struct Unnamer<'db> {
    interner: DbInterner<'db>,
    params: Vec<Option<GenericParamId>>,
}

impl<'db> TypeFolder<DbInterner<'db>> for Unnamer<'db> {
    fn cx(&self) -> DbInterner<'db> {
        self.interner
    }

    fn fold_ty(&mut self, t: Ty<'db>) -> Ty<'db> {
        // A fresh variable is no inference variable: `has_infer` does not see it.
        if !t.has_type_flags(TypeFlags::HAS_TY_FRESH | TypeFlags::HAS_CT_FRESH) {
            return t;
        }
        match t.kind() {
            TyKind::Infer(InferTy::FreshTy(index)) => match self.params[index as usize] {
                Some(GenericParamId::TypeParamId(id)) => {
                    let index = crate::type_or_const_param_idx(self.interner.db, id.into());
                    Ty::new_param(self.interner, id, index)
                }
                _ => Ty::new_error(self.interner, ErrorGuaranteed),
            },
            _ => t.super_fold_with(self),
        }
    }

    fn fold_const(&mut self, c: Const<'db>) -> Const<'db> {
        if !c.has_type_flags(TypeFlags::HAS_TY_FRESH | TypeFlags::HAS_CT_FRESH) {
            return c;
        }
        match c.kind() {
            ConstKind::Infer(InferConst::Fresh(index)) => match self.params[index as usize] {
                Some(GenericParamId::ConstParamId(id)) => {
                    let index = crate::type_or_const_param_idx(self.interner.db, id.into());
                    Const::new_param(self.interner, ParamConst { id, index })
                }
                _ => Const::new_error(self.interner, ErrorGuaranteed),
            },
            _ => c.super_fold_with(self),
        }
    }
}
