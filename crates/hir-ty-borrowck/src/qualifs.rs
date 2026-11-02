//! The qualifs of a value, as rustc's `check_consts::qualifs`: whether the value needs dropping
//! (`NeedsDrop`) and whether it has interior mutability (`HasMutInterior`). Promotion asks them
//! of the value a temporary holds rather than of its type, so `&[OPEN_PAREN, INDENT]` is
//! promoted though `OPEN_PAREN`'s type has a `Cell` in another variant, and `&Signals::EMPTY`
//! is though `Signals` holds an `Arc` it would drop.

use base_db::Db;
use hir_def::{
    AdtId, ConstId, DefWithBodyId, ItemContainerId, Lookup,
    expr_store::{Body, ExpressionStore},
    hir::{Array, Expr, ExprId, RecordSpread, Statement, UnaryOp},
    resolver::{HasResolver, ValueNs},
    signatures::ImplSignature,
};
use std::cell::RefCell;

use crate::mem::FMap;
use rustc_type_ir::inherent::IntoKind;

use hir_ty::{
    Adjust, InferBodyId, InferenceResult,
    db::HirDatabase,
    method_resolution::CandidateId,
    next_solver::{ParamEnv, Ty, TyKind, infer::InferCtxt},
};

/// rustc's `ConstQualifs`, of the two qualifs promotion asks.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct Qualifs {
    pub(crate) needs_drop: bool,
    pub(crate) mut_interior: bool,
}

impl Qualifs {
    pub(crate) fn is_empty(self) -> bool {
        !self.needs_drop && !self.mut_interior
    }

    fn or(self, other: Self) -> Self {
        Qualifs {
            needs_drop: self.needs_drop || other.needs_drop,
            mut_interior: self.mut_interior || other.mut_interior,
        }
    }

    fn and(self, other: Self) -> Self {
        Qualifs {
            needs_drop: self.needs_drop && other.needs_drop,
            mut_interior: self.mut_interior && other.mut_interior,
        }
    }
}

/// A body whose values are asked their qualifs: the one borrow checked, or a `const` item's.
pub(crate) struct ValueCx<'a, 'f, 'db> {
    pub(crate) db: &'db Db,
    pub(crate) infcx: &'a InferCtxt<'db>,
    pub(crate) env: ParamEnv<'db>,
    pub(crate) owner: InferBodyId<'db>,
    pub(crate) store: &'a ExpressionStore,
    pub(crate) infer: &'a InferenceResult<'db>,
    /// What each `const` item's value has, asked once as rustc's `mir_const_qualif` query.
    pub(crate) consts: &'a RefCell<FMap<'f, ConstId, Qualifs>>,
}

/// rustc stops at no depth; this does where a value nests deeper than any real one.
const MAX_DEPTH: usize = 32;

impl<'db> ValueCx<'_, '_, 'db> {
    /// rustc's `in_any_value_of_ty`.
    fn of_ty(&self, ty: Ty<'db>) -> Qualifs {
        Qualifs {
            needs_drop: hir_ty::drop::has_drop_glue(self.infcx, ty, self.env)
                != hir_ty::drop::DropGlue::None,
            mut_interior: !hir_ty::drop::is_freeze(self.infcx, ty, self.env),
        }
    }

    /// The qualifs of the value `expr` makes, rustc's `in_rvalue` and `in_operand` over the
    /// MIR it lowers to: an aggregate has its parts', a borrow the place's it borrows, a
    /// named `const` item what its own value has (rustc's `mir_const_qualif`) where its type
    /// may, and what is not built of parts (a call, a literal) any value of its type's.
    pub(crate) fn value(&self, expr: ExprId, depth: usize) -> Qualifs {
        let ty = self.infer.expr_ty(expr);
        if depth > MAX_DEPTH {
            return self.of_ty(ty);
        }
        if let Some(adjustments) = self.infer.expr_adjustment(expr)
            && let Some(last) = adjustments.last()
        {
            // A cast keeps what its operand has, and an autoref is rustc's `Rvalue::Ref` of
            // the temporary holding the value, which has what the value has (`Droppy.get()`
            // borrows a `Droppy` that needs dropping), or none for a static's.
            if !adjustments
                .iter()
                .all(|it| matches!(it.kind, Adjust::Pointer(_) | Adjust::Borrow(_)))
            {
                return self.of_ty(last.target.as_ref());
            }
            if adjustments.iter().any(|it| matches!(it.kind, Adjust::Borrow(_)))
                && self.static_item(expr)
            {
                return Qualifs::default();
            }
        }
        match self.store.expr(expr) {
            Expr::Path(_) => {
                if self.infer.variant_resolution_for_expr(expr).is_some()
                    && !matches!(ty.kind(), TyKind::FnDef(..))
                {
                    return self.aggregate(ty, std::iter::empty(), depth);
                }
                let of_ty = self.of_ty(ty);
                match self.const_item(expr) {
                    Some(konst) if !of_ty.is_empty() => self.const_value(konst).and(of_ty),
                    _ => of_ty,
                }
            }
            Expr::Tuple { exprs } | Expr::Array(Array::ElementList { elements: exprs }) => {
                exprs.iter().fold(Qualifs::default(), |acc, &it| acc.or(self.value(it, depth + 1)))
            }
            Expr::Array(Array::Repeat { initializer, .. }) => self.value(initializer, depth + 1),
            Expr::RecordLit { fields, spread: RecordSpread::None, .. } => {
                self.aggregate(ty, fields.iter().map(|it| it.expr), depth)
            }
            Expr::Call { callee, args }
                if matches!(self.infer.expr_ty(callee).kind(), TyKind::FnDef(def, _)
                    if !matches!(def.0, hir_def::CallableDefId::FunctionId(_))) =>
            {
                self.aggregate(ty, args.iter().copied(), depth)
            }
            Expr::Ref { expr: inner, .. } => self.borrowed(inner, depth + 1),
            Expr::Cast { expr: inner, .. }
            | Expr::UnaryOp { expr: inner, op: UnaryOp::Not | UnaryOp::Neg } => {
                self.value(inner, depth + 1)
            }
            Expr::BinaryOp { lhs, rhs, op: Some(op) }
                if !matches!(op, hir_def::hir::BinaryOp::Assignment { .. }) =>
            {
                self.value(lhs, depth + 1).or(self.value(rhs, depth + 1))
            }
            // A block of items and a tail is its tail in MIR.
            Expr::Block { statements, tail: Some(tail), .. }
                if statements.iter().all(|it| matches!(it, Statement::Item(_))) =>
            {
                self.value(tail, depth + 1)
            }
            Expr::Field { .. } | Expr::UnaryOp { op: UnaryOp::Deref, .. } | Expr::Index { .. } => {
                self.place(expr, depth)
            }
            _ => self.of_ty(ty),
        }
    }

    /// rustc's `Rvalue::Aggregate`: a struct or variant is made of its fields unless it is
    /// a union, or its type has a `Drop` impl (for `NeedsDrop`) or is `UnsafeCell` (for
    /// `HasMutInterior`), which have what any value of the type has.
    fn aggregate(
        &self,
        ty: Ty<'db>,
        parts: impl Iterator<Item = ExprId>,
        depth: usize,
    ) -> Qualifs {
        let parts =
            || parts.fold(Qualifs::default(), |acc, it| acc.or(self.value(it, depth + 1)));
        let TyKind::Adt(def, _) = ty.kind() else { return parts() };
        let id = def.def_id();
        if matches!(id, AdtId::UnionId(_)) {
            return self.of_ty(ty);
        }
        let has_dtor = hir_ty::drop::destructor(self.db, id).is_some();
        let unsafe_cell = matches!(id, AdtId::StructId(it)
            if Some(it) == self.infcx.interner.lang_items().UnsafeCell);
        if !has_dtor && !unsafe_cell {
            return parts();
        }
        let of_ty = self.of_ty(ty);
        let parts = parts();
        Qualifs {
            needs_drop: if has_dtor { of_ty.needs_drop } else { parts.needs_drop },
            mut_interior: if unsafe_cell { of_ty.mut_interior } else { parts.mut_interior },
        }
    }

    /// rustc's `Rvalue::Ref`: the qualifs of the place borrowed, but a reborrow `&*r` of a
    /// reference is a copy of `r`, and `&STATIC` is one of the constant pointer to the static.
    fn borrowed(&self, place: ExprId, depth: usize) -> Qualifs {
        if let Expr::UnaryOp { expr: base, op: UnaryOp::Deref } = self.store.expr(place)
            && self.infer.expr_adjustment(base).is_none_or(|it| it.is_empty())
            && matches!(self.infer.expr_ty(base).kind(), TyKind::Ref(..))
        {
            return self.value(base, depth + 1);
        }
        if self.static_item(place) {
            return Qualifs::default();
        }
        self.place(place, depth)
    }

    /// rustc's `in_place`: a projection has nothing its type cannot have, a dereference
    /// what any value of its type has, and otherwise the qualifs of what it projects.
    fn place(&self, expr: ExprId, depth: usize) -> Qualifs {
        let ty = self.infer.expr_ty(expr);
        if depth > MAX_DEPTH || self.infer.expr_adjustment(expr).is_some_and(|it| !it.is_empty()) {
            return self.of_ty(ty);
        }
        match self.store.expr(expr) {
            Expr::Field { expr: base, .. } | Expr::Index { base, .. } => {
                self.of_ty(ty).and(self.place(base, depth + 1))
            }
            Expr::UnaryOp { op: UnaryOp::Deref, .. } => self.of_ty(ty),
            _ => self.value(expr, depth + 1),
        }
    }

    fn resolve(&self, expr: ExprId) -> Option<ValueNs> {
        let Expr::Path(path) = self.store.expr(expr) else { return None };
        let mut resolver = self.owner.resolver(self.db);
        let _guard = resolver.update_to_inner_scope(
            self.db,
            self.owner.expression_store_owner(self.db),
            expr,
        );
        let hygiene = self.store.expr_path_hygiene(expr);
        resolver.resolve_path_in_value_ns_fully(self.db, path, hygiene)
    }

    /// The `const` item `expr` names whose value rustc asks its qualifs of: one not of a
    /// trait, which rustc's `in_operand` takes to have what any value of its type has.
    fn const_item(&self, expr: ExprId) -> Option<ConstId> {
        let konst = match self.infer.assoc_resolutions_for_expr(expr) {
            Some((CandidateId::ConstId(it), _)) => it,
            Some(_) => return None,
            None => match self.resolve(expr)? {
                ValueNs::ConstId(it) => it,
                _ => return None,
            },
        };
        match konst.lookup(self.db).container {
            ItemContainerId::ModuleId(_) => Some(konst),
            ItemContainerId::ImplId(it) => {
                ImplSignature::of(self.db, it).target_trait.is_none().then_some(konst)
            }
            _ => None,
        }
    }

    fn static_item(&self, expr: ExprId) -> bool {
        self.infer.assoc_resolutions_for_expr(expr).is_none()
            && matches!(self.resolve(expr), Some(ValueNs::StaticId(_)))
    }

    /// The qualifs of a `const` item's value, rustc's `mir_const_qualif`. A const whose value
    /// names itself, an error rustc reports as a query cycle, has every qualif.
    fn const_value(&self, konst: ConstId) -> Qualifs {
        if let Some(&it) = self.consts.borrow().get(&konst) {
            return it;
        }
        self.consts.borrow_mut().insert(konst, Qualifs { needs_drop: true, mut_interior: true });
        let def = DefWithBodyId::ConstId(konst);
        let body = Body::of(self.db, def);
        let cx = ValueCx {
            db: self.db,
            infcx: self.infcx,
            env: self.db.trait_environment(konst.into()),
            owner: InferBodyId::DefWithBodyId(def),
            store: body,
            infer: InferenceResult::of(self.db, def),
            consts: self.consts,
        };
        let qualifs = cx.value(body.root_expr(), 0);
        self.consts.borrow_mut().insert(konst, qualifs);
        qualifs
    }
}
