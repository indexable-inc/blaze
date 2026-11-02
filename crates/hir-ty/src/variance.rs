//! Module for inferring the variance of type and lifetime parameters. See the [rustc dev guide]
//! chapter for more info.
//!
//! [rustc dev guide]: https://rustc-dev-guide.rust-lang.org/variance.html
//!
//! The implementation here differs from rustc. Rustc does a crate wide fixpoint resolution
//! as the algorithm for determining variance is a fixpoint computation with potential cycles that
//! need to be resolved. rust-analyzer does not want a crate-wide analysis though as that would hurt
//! incrementality too much and as such our query is based on a per item basis.
//!
//! Per item, the queries can form cycles; the query engine resolves them by fixpoint iteration
//! (`variances_of`'s `cycle_initial` and `cycle_fn`).

mod unused;

use base_db::Db;
pub use unused::adt_unused_params;

use hir_def::{
    AdtId, GenericDefId, GenericParamId, VariantId,
    signatures::{StructFlags, StructSignature},
};
use rustc_ast_ir::Mutability;
use rustc_type_ir::{TypeFlags, TypeVisitableExt as _, Variance, inherent::IntoKind};

use crate::{
    CallableDefId,
    db::HirDatabase,
    generics::{Generics, generics},
    next_solver::{
        Const, ConstKind, ExistentialPredicate, GenericArgKind, GenericArgs, Pattern, PatternKind,
        Region, RegionKind, StoredVariancesOf, TermKind, Ty, TyKind, VariancesOf,
    },
};

pub(crate) fn variances_of(db: &Db, def: GenericDefId) -> VariancesOf<'_> {
    variances_of_query(db, def).as_ref()
}

#[blaze_query::tracked(
    returns(ref),
    cycle_fn = crate::variance::variances_of_cycle_fn,
    cycle_initial = crate::variance::variances_of_cycle_initial,
    mem_tag = Infer,
)]
fn variances_of_query(db: &Db, def: GenericDefId) -> StoredVariancesOf {
    tracing::debug!("variances_of(def={:?})", def);
    match def {
        GenericDefId::FunctionId(_) => (),
        GenericDefId::AdtId(adt) => {
            if let AdtId::StructId(id) = adt {
                let flags = &StructSignature::of(db, id).flags;
                let types = || crate::next_solver::default_types();
                if flags.contains(StructFlags::IS_UNSAFE_CELL) {
                    return types().one_invariant.store();
                } else if flags.intersects(
                    StructFlags::IS_PHANTOM_DATA | StructFlags::IS_COVARIANT_UNSAFE_CELL,
                ) {
                    return types().one_covariant.store();
                }
            }
        }
        _ => return VariancesOf::empty().store(),
    }

    let generics = generics(db, def);
    let count = generics.len(true);
    if count == 0 {
        return VariancesOf::empty().store();
    }
    blaze_mem::frame::current(|frame| {
        let variances = frame.alloc_slice_fill_with(count, |_| Variance::Bivariant);
        VariancesOf::new_from_slice(Context { generics, variances, db }.solve()).store()
    })
}

/// The variances of a tuple struct's or tuple variant's constructor function item, as rustc's
/// `variances_of` gives them for a `DefKind::Ctor`: inferred from the constructor's signature
/// `fn(fields..) -> Adt<..>`, not from the ADT's. A parameter its fields use appears both as an
/// argument and in the result, so it is invariant (`SomeStruct::<&'static u32>` can't be called
/// with a shorter borrow); one only another variant uses keeps the ADT's variance.
pub fn ctor_variances(db: &Db, ctor: VariantId) -> VariancesOf<'_> {
    ctor_variances_query(db, ctor).as_ref()
}

#[blaze_query::tracked(returns(ref), mem_tag = Infer)]
fn ctor_variances_query(db: &Db, ctor: VariantId) -> StoredVariancesOf {
    let adt: GenericDefId = match ctor {
        VariantId::StructId(s) => AdtId::StructId(s).into(),
        VariantId::EnumVariantId(v) => AdtId::EnumId(v.loc(db).parent).into(),
        VariantId::UnionId(_) => return VariancesOf::empty().store(),
    };
    let generics = generics(db, adt);
    let count = generics.len(true);
    if count == 0 {
        return VariancesOf::empty().store();
    }
    blaze_mem::frame::current(|frame| {
        let variances = frame.alloc_slice_fill_with(count, |_| Variance::Bivariant);
        let mut cx = Context { generics, variances, db };
        let callable = match ctor {
            VariantId::StructId(s) => CallableDefId::StructId(s),
            VariantId::EnumVariantId(v) => CallableDefId::EnumVariantId(v),
            VariantId::UnionId(_) => unreachable!(),
        };
        let sig = db.callable_item_signature(callable).instantiate_identity().skip_binder();
        cx.add_constraints_from_sig(sig.inputs_and_output.iter(), Variance::Covariant);
        let variances = cx.variances;
        for (idx, param) in cx.generics.iter_id(false).enumerate() {
            if let GenericParamId::ConstParamId(_) = param {
                variances[idx] = Variance::Invariant;
            }
        }
        VariancesOf::new_from_slice(variances).store()
    })
}

pub(crate) fn variances_of_cycle_fn(
    _db: &Db,
    _: &blaze_query::db::Cycle<'_>,
    _last_provisional_value: &StoredVariancesOf,
    value: StoredVariancesOf,
    _def: GenericDefId,
) -> StoredVariancesOf {
    value
}

fn glb(v1: Variance, v2: Variance) -> Variance {
    // Greatest lower bound of the variance lattice as defined in The Paper:
    //
    //       *
    //    -     +
    //       o
    match (v1, v2) {
        (Variance::Invariant, _) | (_, Variance::Invariant) => Variance::Invariant,

        (Variance::Covariant, Variance::Contravariant) => Variance::Invariant,
        (Variance::Contravariant, Variance::Covariant) => Variance::Invariant,

        (Variance::Covariant, Variance::Covariant) => Variance::Covariant,

        (Variance::Contravariant, Variance::Contravariant) => Variance::Contravariant,

        (x, Variance::Bivariant) | (Variance::Bivariant, x) => x,
    }
}

pub(crate) fn variances_of_cycle_initial(
    db: &Db,
    _: blaze_query::db::Id,
    def: GenericDefId,
) -> StoredVariancesOf {
    let generics = generics(db, def);
    let count = generics.len(true);

    VariancesOf::new_from_iter(std::iter::repeat_n(Variance::Bivariant, count)).store()
}

struct Context<'db, 'f> {
    db: &'db Db,
    generics: Generics<'db>,
    /// The variances while they are worked out, one byte a parameter, in the run's frame.
    variances: &'f mut [Variance],
}

impl<'db, 'f> Context<'db, 'f> {
    fn solve(mut self) -> &'f mut [Variance] {
        tracing::debug!("solve(generics={:?})", self.generics);
        match self.generics.def() {
            GenericDefId::AdtId(adt) => {
                let db = self.db;
                let mut add_constraints_from_variant = |variant| {
                    for (_, field) in db.field_types(variant).iter() {
                        self.add_constraints_from_ty(
                            field.ty().instantiate_identity().skip_norm_wip(),
                            Variance::Covariant,
                        );
                    }
                };
                match adt {
                    AdtId::StructId(s) => add_constraints_from_variant(VariantId::StructId(s)),
                    AdtId::UnionId(u) => add_constraints_from_variant(VariantId::UnionId(u)),
                    AdtId::EnumId(e) => {
                        e.enum_variants(db).variants.values().for_each(|&(variant, _)| {
                            add_constraints_from_variant(VariantId::EnumVariantId(variant))
                        });
                    }
                }
            }
            GenericDefId::FunctionId(f) => {
                let sig =
                    self.db.callable_item_signature(f.into()).instantiate_identity().skip_binder();
                self.add_constraints_from_sig(sig.inputs_and_output.iter(), Variance::Covariant);
            }
            _ => {}
        }
        let variances = self.variances;

        // Const parameters are always invariant.
        // Make all const parameters invariant.
        for (idx, param) in self.generics.iter_id(false).enumerate() {
            if let GenericParamId::ConstParamId(_) = param {
                variances[idx] = Variance::Invariant;
            }
        }

        // Functions are permitted to have unused generic parameters: make those invariant.
        if let GenericDefId::FunctionId(_) = self.generics.def() {
            variances
                .iter_mut()
                .filter(|&&mut v| v == Variance::Bivariant)
                .for_each(|v| *v = Variance::Invariant);
        }

        variances
    }

    /// Adds constraints appropriate for an instance of `ty` appearing
    /// in a context with the generics defined in `generics` and
    /// ambient variance `variance`
    fn add_constraints_from_ty(&mut self, ty: Ty<'db>, variance: Variance) {
        // Only a type parameter and an early-bound lifetime are constrained: a type with
        // neither in it (`u8`, `Vec<String>`) adds nothing, and its ADTs' variances need not
        // be read.
        if !ty.has_type_flags(TypeFlags::HAS_TY_PARAM.union(TypeFlags::HAS_RE_PARAM)) {
            return;
        }
        tracing::debug!("add_constraints_from_ty(ty={:?}, variance={:?})", ty, variance);
        match ty.kind() {
            TyKind::Int(_)
            | TyKind::Uint(_)
            | TyKind::Float(_)
            | TyKind::Char
            | TyKind::Bool
            | TyKind::Never
            | TyKind::Str
            | TyKind::Foreign(..) => {
                // leaf type -- noop
            }
            TyKind::FnDef(..)
            | TyKind::Coroutine(..)
            | TyKind::CoroutineClosure(..)
            | TyKind::Closure(..) => {
                unreachable!("Unexpected unnameable type in variance computation: {:?}", ty);
            }
            TyKind::Ref(lifetime, ty, mutbl) => {
                self.add_constraints_from_region(lifetime, variance);
                self.add_constraints_from_mt(ty, mutbl, variance);
            }
            TyKind::Array(typ, len) => {
                self.add_constraints_from_const(len);
                self.add_constraints_from_ty(typ, variance);
            }
            TyKind::Slice(typ) => {
                self.add_constraints_from_ty(typ, variance);
            }
            TyKind::RawPtr(ty, mutbl) => {
                self.add_constraints_from_mt(ty, mutbl, variance);
            }
            TyKind::Tuple(subtys) => {
                for subty in subtys {
                    self.add_constraints_from_ty(subty, variance);
                }
            }
            TyKind::Adt(def, args) => {
                self.add_constraints_from_args(def.def_id().into(), args, variance);
            }
            TyKind::Alias(_, alias) => {
                // FIXME: Probably not correct wrt. opaques.
                self.add_constraints_from_invariant_args(alias.args);
            }
            TyKind::Dynamic(bounds, region) => {
                // The type `dyn Trait<T> +'a` is covariant w/r/t `'a`:
                self.add_constraints_from_region(region, variance);

                for bound in bounds {
                    match bound.skip_binder() {
                        ExistentialPredicate::Trait(trait_ref) => {
                            self.add_constraints_from_invariant_args(trait_ref.args)
                        }
                        ExistentialPredicate::Projection(projection) => {
                            self.add_constraints_from_invariant_args(projection.args);
                            match projection.term.kind() {
                                TermKind::Ty(ty) => {
                                    self.add_constraints_from_ty(ty, Variance::Invariant)
                                }
                                TermKind::Const(konst) => self.add_constraints_from_const(konst),
                            }
                        }
                        ExistentialPredicate::AutoTrait(_) => {}
                    }
                }
            }

            // Chalk has no params, so use placeholders for now?
            TyKind::Param(param) => self.constrain(param.index as usize, variance),
            TyKind::FnPtr(sig, _) => {
                self.add_constraints_from_sig(sig.skip_binder().inputs_and_output.iter(), variance);
            }
            TyKind::Error(_) => {
                // we encounter this when walking the trait references for object
                // types, where we use Error as the Self type
            }
            TyKind::Pat(typ, pat) => {
                self.add_constraints_from_pat(pat);
                self.add_constraints_from_ty(typ, variance);
            }
            TyKind::Bound(..) => {}
            TyKind::CoroutineWitness(..)
            | TyKind::Placeholder(..)
            | TyKind::Infer(..)
            | TyKind::UnsafeBinder(..) => {
                unreachable!("unexpected type encountered in variance inference: {:?}", ty)
            }
        }
    }

    fn add_constraints_from_pat(&mut self, pat: Pattern<'db>) {
        match pat.kind() {
            PatternKind::Range { start, end } => {
                self.add_constraints_from_const(start);
                self.add_constraints_from_const(end);
            }
            PatternKind::NotNull => {}
            PatternKind::Or(patterns) => {
                for pat in patterns {
                    self.add_constraints_from_pat(pat)
                }
            }
        }
    }

    fn add_constraints_from_invariant_args(&mut self, args: GenericArgs<'db>) {
        for k in args.iter() {
            match k.kind() {
                GenericArgKind::Lifetime(lt) => {
                    self.add_constraints_from_region(lt, Variance::Invariant)
                }
                GenericArgKind::Type(ty) => self.add_constraints_from_ty(ty, Variance::Invariant),
                GenericArgKind::Const(val) => self.add_constraints_from_const(val),
            }
        }
    }

    /// Adds constraints appropriate for a nominal type (enum, struct,
    /// object, etc) appearing in a context with ambient variance `variance`
    fn add_constraints_from_args(
        &mut self,
        def_id: GenericDefId,
        args: GenericArgs<'db>,
        variance: Variance,
    ) {
        if args.is_empty() {
            return;
        }
        let variances = self.db.variances_of(def_id);

        for (k, v) in args.iter().zip(variances) {
            match k.kind() {
                GenericArgKind::Lifetime(lt) => {
                    self.add_constraints_from_region(lt, variance.xform(v))
                }
                GenericArgKind::Type(ty) => self.add_constraints_from_ty(ty, variance.xform(v)),
                GenericArgKind::Const(val) => self.add_constraints_from_const(val),
            }
        }
    }

    /// Adds constraints appropriate for a const expression `val`
    /// in a context with ambient variance `variance`
    fn add_constraints_from_const(&mut self, c: Const<'db>) {
        match c.kind() {
            ConstKind::Alias(_, c) => self.add_constraints_from_invariant_args(c.args),
            _ => {}
        }
    }

    /// Adds constraints appropriate for a function with signature
    /// `sig` appearing in a context with ambient variance `variance`
    fn add_constraints_from_sig(
        &mut self,
        mut sig_tys: impl DoubleEndedIterator<Item = Ty<'db>>,
        variance: Variance,
    ) {
        let contra = variance.xform(Variance::Contravariant);
        let Some(output) = sig_tys.next_back() else {
            unreachable!("function signature has no return type");
        };
        self.add_constraints_from_ty(output, variance);
        for input in sig_tys {
            self.add_constraints_from_ty(input, contra);
        }
    }

    /// Adds constraints appropriate for a region appearing in a
    /// context with ambient variance `variance`
    fn add_constraints_from_region(&mut self, region: Region<'db>, variance: Variance) {
        tracing::debug!(
            "add_constraints_from_region(region={:?}, variance={:?})",
            region,
            variance
        );
        match region.kind() {
            RegionKind::ReEarlyParam(param) => self.constrain(param.index as usize, variance),
            RegionKind::ReStatic => {}
            RegionKind::ReBound(..) => {
                // Either a higher-ranked region inside of a type or a
                // late-bound function parameter.
                //
                // We do not compute constraints for either of these.
            }
            RegionKind::ReError(_) => {}
            RegionKind::ReLateParam(..)
            | RegionKind::RePlaceholder(..)
            | RegionKind::ReVar(..)
            | RegionKind::ReErased => {
                // We don't expect to see anything but 'static or bound
                // regions when visiting member types or method types.
                unreachable!(
                    "unexpected region encountered in variance \
                      inference: {:?}",
                    region
                );
            }
        }
    }

    /// Adds constraints appropriate for a mutability-type pair
    /// appearing in a context with ambient variance `variance`
    fn add_constraints_from_mt(&mut self, ty: Ty<'db>, mt: Mutability, variance: Variance) {
        self.add_constraints_from_ty(
            ty,
            match mt {
                Mutability::Mut => Variance::Invariant,
                Mutability::Not => variance,
            },
        );
    }

    fn constrain(&mut self, index: usize, variance: Variance) {
        tracing::debug!(
            "constrain(index={:?}, variance={:?}, to={:?})",
            index,
            self.variances[index],
            variance
        );
        self.variances[index] = glb(self.variances[index], variance);
    }
}
