//! rustc's `InhabitedPredicate`: when a type is inhabited, as a formula over the modules it is
//! seen from, the generic types it holds and the consts of its array lengths, simplified as
//! rustc simplifies it, so that "is it `True`" answers as rustc's does.
//!
//! Follows rustc's `compiler/rustc_middle/src/ty/inhabitedness/mod.rs` (`Ty::inhabited_predicate`,
//! `inhabited_predicate_adt`, `VariantDef::inhabited_predicate`) and
//! `compiler/rustc_middle/src/ty/inhabitedness/inhabited_predicate.rs` (MIT or Apache-2.0).

use base_db::Db;
use blaze_hash::HashMap;
use hir::db::HirDatabase;
use hir_def::{
    AdtId, AttrDefId, HasModule as _, ModuleId, VariantId,
    attrs::AttrFlags,
    nameres::crate_def_map,
    signatures::VariantFields,
    visibility::{Visibility, VisibilityExplicitness},
};
use hir_ty::{
    ImplTraitId,
    consteval::try_const_usize,
    next_solver::{
        AliasTy, Const, DbInterner, EarlyBinder, GenericArgs, Ty, TyKind,
        infer::traits::ObligationCause,
    },
};
use rustc_type_ir::{AliasTyKind, inherent::IntoKind as _};
use smallvec::SmallVec;

use crate::lints::bodies::types::util::Solver;

/// One of rustc's `InhabitedPredicate`s; `And` and `Or` name a pair in [`Preds`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Pred<'db> {
    True,
    False,
    /// Uninhabited when the length is not zero.
    ConstIsZero(Const<'db>),
    /// Uninhabited inside the module.
    NotInModule(ModuleId),
    GenericType(Ty<'db>),
    /// A local opaque type: inhabited unless its hidden type is known not to be.
    OpaqueType(Ty<'db>),
    And(u32),
    Or(u32),
}

/// The predicates of one lint's types: their `And`s and `Or`s, and those of the ADTs seen.
pub(super) struct Preds<'s, 'db> {
    solver: &'s Solver<'db>,
    db: &'db Db,
    krate: base_db::Crate,
    pairs: Vec<[Pred<'db>; 2]>,
    adts: HashMap<AdtId, Pred<'db>>,
}

impl<'s, 'db> Preds<'s, 'db> {
    /// `krate` is the crate being linted: an enum of another with `#[non_exhaustive]` is
    /// inhabited, and only local opaque types are looked into.
    pub(super) fn new(solver: &'s Solver<'db>, krate: base_db::Crate) -> Preds<'s, 'db> {
        let db = solver.interner().db;
        // alloc: empty, pushed to only for an `And` or `Or` of a type asked about
        Preds { solver, db, krate, pairs: Vec::new(), adts: HashMap::default() }
    }

    fn interner(&self) -> DbInterner<'db> {
        self.solver.interner()
    }

    /// rustc's `Ty::inhabited_predicate`.
    pub(super) fn ty(&mut self, ty: Ty<'db>) -> Pred<'db> {
        match ty.kind() {
            TyKind::Adt(def, _) if def.is_union() => Pred::True,
            TyKind::Adt(def, _) if self.applicable_non_exhaustive(def.def_id()) => Pred::True,
            TyKind::Never => Pred::False,
            TyKind::Param(_) => Pred::GenericType(ty),
            TyKind::Alias(_, AliasTy { kind, .. }) => match kind {
                AliasTyKind::Projection { .. } | AliasTyKind::Inherent { .. } | AliasTyKind::Free { .. } => {
                    Pred::GenericType(ty)
                }
                AliasTyKind::Opaque { def_id } => {
                    let krate = match def_id.0.loc(self.db) {
                        ImplTraitId::ReturnTypeImplTrait(it, _) => it.krate(self.db),
                        ImplTraitId::TypeAliasImplTrait(it, _) => it.krate(self.db),
                    };
                    if krate == self.krate { Pred::OpaqueType(ty) } else { Pred::True }
                }
            },
            TyKind::Tuple(tys) if tys.is_empty() => Pred::True,
            TyKind::Adt(def, args) => {
                let adt = self.adt(def.def_id());
                self.instantiate(adt, args)
            }
            TyKind::Tuple(tys) => {
                let mut result = Pred::True;
                for field in tys.iter() {
                    let pred = self.ty(field);
                    if pred == Pred::False {
                        return Pred::False;
                    }
                    result = self.and(result, pred);
                }
                result
            }
            TyKind::Array(elem, len) => match try_const_usize(self.db, len) {
                Some(0) => Pred::True,
                Some(_) => self.ty(elem),
                None => {
                    let elem = self.ty(elem);
                    self.or(elem, Pred::ConstIsZero(len))
                }
            },
            _ => Pred::True,
        }
    }

    /// rustc's `inhabited_predicate_adt`: any of its variants, of its own generic parameters.
    fn adt(&mut self, adt: AdtId) -> Pred<'db> {
        if let Some(&pred) = self.adts.get(&adt) {
            return pred;
        }
        // An ADT inside itself is an infinitely sized type, which is an error of its own.
        self.adts.insert(adt, Pred::True);
        let variants: SmallVec<[VariantId; 4]> = match adt {
            // alloc: inline, a `SmallVec` of one allocates nothing
            AdtId::StructId(it) => [VariantId::StructId(it)].into_iter().collect(),
            // alloc: inline, a `SmallVec` of one allocates nothing
            AdtId::UnionId(it) => [VariantId::UnionId(it)].into_iter().collect(),
            AdtId::EnumId(it) => {
                // alloc: inline but for an enum of more than four variants, once per ADT (kept in `adts`)
                it.enum_variants(self.db).variants.values().map(|&(it, _)| VariantId::EnumVariantId(it)).collect()
            }
        };
        let is_enum = matches!(adt, AdtId::EnumId(_));
        let mut result = Pred::False;
        for variant in variants {
            let pred = self.variant(variant, is_enum);
            if pred == Pred::True {
                result = Pred::True;
                break;
            }
            result = self.or(result, pred);
        }
        self.adts.insert(adt, result);
        result
    }

    /// rustc's `VariantDef::inhabited_predicate`, of the ADT's own generic parameters: all of
    /// its fields, a struct's field hidden from where it is not visible.
    pub(super) fn variant(&mut self, variant: VariantId, is_enum: bool) -> Pred<'db> {
        let db = self.db;
        let fields = variant.fields(db).fields();
        let types = db.field_types(variant);
        let visibilities = if is_enum { None } else { Some(VariantFields::field_visibilities(db, variant)) };
        let mut result = Pred::True;
        for (field, _) in fields.iter() {
            let pred = self.ty(types[field].ty().skip_binder());
            let pred = match visibilities.map(|it| it[field]) {
                None | Some(Visibility::Public) => pred,
                Some(Visibility::Module(module, _)) => self.or(Pred::NotInModule(module), pred),
                Some(Visibility::PubCrate(krate)) => {
                    self.or(Pred::NotInModule(crate_def_map(db, krate).root_module_id()), pred)
                }
            };
            if pred == Pred::False {
                return Pred::False;
            }
            result = self.and(result, pred);
        }
        result
    }

    /// rustc's `variant_list_has_applicable_non_exhaustive`: a `#[non_exhaustive]` enum of
    /// another crate.
    fn applicable_non_exhaustive(&self, adt: AdtId) -> bool {
        let AdtId::EnumId(it) = adt else { return false };
        it.krate(self.db) != self.krate
            && AttrFlags::query(self.db, AttrDefId::AdtId(adt)).contains(AttrFlags::NON_EXHAUSTIVE)
    }

    pub(super) fn and(&mut self, a: Pred<'db>, b: Pred<'db>) -> Pred<'db> {
        match self.reduce_and(a, b) {
            Some(it) => it,
            None => Pred::And(self.pair(a, b)),
        }
    }

    pub(super) fn or(&mut self, a: Pred<'db>, b: Pred<'db>) -> Pred<'db> {
        match self.reduce_or(a, b) {
            Some(it) => it,
            None => Pred::Or(self.pair(a, b)),
        }
    }

    fn pair(&mut self, a: Pred<'db>, b: Pred<'db>) -> u32 {
        self.pairs.push([a, b]);
        (self.pairs.len() - 1) as u32
    }

    /// `a` is inside `b`.
    fn is_descendant_of(&self, a: ModuleId, b: ModuleId) -> bool {
        Visibility::Module(b, VisibilityExplicitness::Explicit).is_visible_from(self.db, a)
    }

    fn reduce_and(&mut self, a: Pred<'db>, b: Pred<'db>) -> Option<Pred<'db>> {
        match (a, b) {
            (Pred::True, it) | (it, Pred::True) => Some(it),
            (Pred::False, _) | (_, Pred::False) => Some(Pred::False),
            (Pred::ConstIsZero(x), Pred::ConstIsZero(y)) if x == y => Some(Pred::ConstIsZero(x)),
            (Pred::NotInModule(x), Pred::NotInModule(y)) if x == y => Some(Pred::NotInModule(x)),
            (Pred::NotInModule(x), Pred::NotInModule(y)) if self.is_descendant_of(x, y) => {
                Some(Pred::NotInModule(y))
            }
            (Pred::NotInModule(x), Pred::NotInModule(y)) if self.is_descendant_of(y, x) => {
                Some(Pred::NotInModule(x))
            }
            (Pred::GenericType(x), Pred::GenericType(y)) if x == y => Some(Pred::GenericType(x)),
            (Pred::And(pair), c) | (c, Pred::And(pair)) => {
                let [x, y] = self.pairs[pair as usize];
                if let Some(xc) = self.reduce_and(x, c) {
                    Some(self.and(xc, y))
                } else if let Some(yc) = self.reduce_and(y, c) {
                    Some(Pred::And(self.pair(x, yc)))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn reduce_or(&mut self, a: Pred<'db>, b: Pred<'db>) -> Option<Pred<'db>> {
        match (a, b) {
            (Pred::True, _) | (_, Pred::True) => Some(Pred::True),
            (Pred::False, it) | (it, Pred::False) => Some(it),
            (Pred::ConstIsZero(x), Pred::ConstIsZero(y)) if x == y => Some(Pred::ConstIsZero(x)),
            (Pred::NotInModule(x), Pred::NotInModule(y)) if x == y => Some(Pred::NotInModule(x)),
            (Pred::NotInModule(x), Pred::NotInModule(y)) if self.is_descendant_of(x, y) => {
                Some(Pred::NotInModule(x))
            }
            (Pred::NotInModule(x), Pred::NotInModule(y)) if self.is_descendant_of(y, x) => {
                Some(Pred::NotInModule(y))
            }
            (Pred::GenericType(x), Pred::GenericType(y)) if x == y => Some(Pred::GenericType(x)),
            (Pred::Or(pair), c) | (c, Pred::Or(pair)) => {
                let [x, y] = self.pairs[pair as usize];
                if let Some(xc) = self.reduce_or(x, c) {
                    Some(self.or(xc, y))
                } else if let Some(yc) = self.reduce_or(y, c) {
                    Some(Pred::Or(self.pair(x, yc)))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// rustc's `InhabitedPredicate::instantiate`: the generic types and consts replaced by
    /// those of `args`.
    pub(super) fn instantiate(&mut self, pred: Pred<'db>, args: GenericArgs<'db>) -> Pred<'db> {
        self.instantiate_opt(pred, args).unwrap_or(pred)
    }

    fn instantiate_opt(&mut self, pred: Pred<'db>, args: GenericArgs<'db>) -> Option<Pred<'db>> {
        let interner = self.interner();
        match pred {
            Pred::ConstIsZero(c) => {
                let c = EarlyBinder::bind(interner, c).instantiate(interner, args).skip_norm_wip();
                Some(match try_const_usize(self.db, c) {
                    Some(0) => Pred::True,
                    Some(_) => Pred::False,
                    None => Pred::ConstIsZero(c),
                })
            }
            Pred::GenericType(t) => {
                let t = EarlyBinder::bind(interner, t).instantiate(interner, args).skip_norm_wip();
                Some(self.ty(t))
            }
            Pred::And(pair) => {
                let [a, b] = self.pairs[pair as usize];
                match self.instantiate_opt(a, args) {
                    None => self.instantiate_opt(b, args).map(|b| self.and(a, b)),
                    Some(Pred::False) => Some(Pred::False),
                    Some(a) => {
                        let b = self.instantiate(b, args);
                        Some(self.and(a, b))
                    }
                }
            }
            Pred::Or(pair) => {
                let [a, b] = self.pairs[pair as usize];
                match self.instantiate_opt(a, args) {
                    None => self.instantiate_opt(b, args).map(|b| self.or(a, b)),
                    Some(Pred::True) => Some(Pred::True),
                    Some(a) => {
                        let b = self.instantiate(b, args);
                        Some(self.or(a, b))
                    }
                }
            }
            // rustc never has an opaque type in an ADT's predicate (`bug!`): nothing to replace.
            Pred::True | Pred::False | Pred::NotInModule(_) | Pred::OpaqueType(_) => None,
        }
    }

    /// rustc's `InhabitedPredicate::apply_any_module`: whether it is inhabited, `None` when that
    /// depends on the module it is seen from.
    pub(super) fn apply_any_module(&mut self, pred: Pred<'db>) -> Option<bool> {
        let mut stack = SmallVec::new();
        self.apply(pred, &mut stack)
    }

    fn apply(&mut self, pred: Pred<'db>, stack: &mut SmallVec<[Ty<'db>; 1]>) -> Option<bool> {
        match pred {
            Pred::False => Some(false),
            Pred::True => Some(true),
            Pred::ConstIsZero(c) => Some(!matches!(try_const_usize(self.db, c), Some(1..))),
            Pred::NotInModule(_) => None,
            Pred::GenericType(t) => {
                let normalized = match self.normalize(t) {
                    Some(t) => self.ty(t),
                    None => pred,
                };
                match normalized {
                    // Nothing known beyond what was: taken as inhabited.
                    Pred::GenericType(_) => Some(true),
                    // A type that is cyclic once instantiated is a layout error of its own.
                    _ if stack.contains(&t) => Some(true),
                    it => {
                        stack.push(t);
                        let result = self.apply(it, stack);
                        stack.pop();
                        result
                    }
                }
            }
            // Its hidden type is not revealed here, as rustc's lints don't reveal it.
            Pred::OpaqueType(_) => Some(true),
            Pred::And(pair) => {
                let [a, b] = self.pairs[pair as usize];
                let a = self.apply(a, stack);
                if a == Some(false) {
                    return Some(false);
                }
                match (a, self.apply(b, stack)) {
                    (_, Some(false)) => Some(false),
                    (Some(true), Some(true)) => Some(true),
                    _ => None,
                }
            }
            Pred::Or(pair) => {
                let [a, b] = self.pairs[pair as usize];
                let a = self.apply(a, stack);
                if a == Some(true) {
                    return Some(true);
                }
                match (a, self.apply(b, stack)) {
                    (_, Some(true)) => Some(true),
                    (Some(false), Some(false)) => Some(false),
                    _ => None,
                }
            }
        }
    }

    /// rustc's `try_normalize_erasing_regions` in the typing environment of the lint.
    pub(super) fn normalize(&self, ty: Ty<'db>) -> Option<Ty<'db>> {
        let cause = ObligationCause::dummy();
        self.solver.infcx.at(&cause, self.solver.env).deeply_normalize(ty).ok()
    }
}
