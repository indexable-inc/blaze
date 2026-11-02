//! Monomorphization of mir, which is used in mir interpreter and const eval.
//!
//! The job of monomorphization is:
//! * Monomorphization. That is, replacing `Option<T>` with `Option<i32>` where `T:=i32` substitution
//!   is provided
//! * Normalizing types, for example replacing RPIT of other functions called in this body.
//!
//! So the monomorphization should be called even if the substitution is empty.

use base_db::Db;
use rustc_next_trait_solver::placeholder::BoundVarReplacer;
use rustc_type_ir::inherent::IntoKind;
use rustc_type_ir::{
    FallibleTypeFolder, Interner, TypeFlags, TypeFoldable, TypeFolder, TypeSuperFoldable,
    TypeVisitableExt, UniverseIndex,
};

use crate::{
    InferBodyId, ParamEnvAndCrate,
    next_solver::{
        Allocation, AllocationData, Const, ConstExt, ConstKind, Region, RegionExt, RegionKind,
        StoredConst, StoredGenericArgs, StoredTy,
    },
    traits::StoredParamEnvAndCrate,
};
use crate::{
    db::{HirDatabase, InternedOpaqueTyId},
    next_solver::{
        AliasTyKind, Binder, DbInterner, GenericArgs, ParamEnv, SolverDefId, Ty, TyKind,
        TypingMode,
        infer::{DbInternerInferExt, InferCtxt, traits::ObligationCause},
        obligation_ctxt::ObligationCtxt,
        references_non_lt_error,
        util::PlaceholderReplacer,
    },
};

use super::{MirBody, MirLowerError, Operand, OperandKind, Rvalue, StatementKind, TerminatorKind};

/// `ty` with a top-level alias normalized in `env`, or `ty` itself where the solver can't
/// normalize it. Almost no type asked of a place or a field is an alias, and one that isn't
/// is given back as the solver would, without a fulfillment context made for it.
pub fn normalize_or_keep<'db>(infcx: &InferCtxt<'db>, env: ParamEnv<'db>, ty: Ty<'db>) -> Ty<'db> {
    if !matches!(ty.kind(), TyKind::Alias(..)) {
        return ty;
    }
    ObligationCtxt::new(infcx).structurally_normalize_ty(&ObligationCause::dummy(), env, ty).unwrap_or(ty)
}

struct Filler<'db> {
    infcx: InferCtxt<'db>,
    trait_env: ParamEnvAndCrate<'db>,
    subst: GenericArgs<'db>,
    /// One entry for each binder the fold is inside of, as normalization wants them. On the
    /// global allocator because the solver's `BoundVarReplacer` takes a `&mut Vec`; it stays
    /// unallocated for a body whose types have no binders.
    universes: Vec<Option<UniverseIndex>>,
    /// How many aliases the fold is normalizing inside of, which rustc's `AssocTypeNormalizer`
    /// stops past the crate's `recursion_limit` with an error.
    depth: usize,
    /// Whether the fold puts `subst` in place of the parameters, else normalizes the aliases,
    /// rustc's instantiate then normalize. Apart, as a generic caller's arguments name its own
    /// parameters, which instantiating again would take for the callee's: `F` of
    /// `goldschmidt::<F::ISet1>` in `sqrt_round<F>` would grow into `<F::ISet1 as ..>::ISet1`.
    instantiating: bool,
}

impl<'db> FallibleTypeFolder<DbInterner<'db>> for Filler<'db> {
    type Error = MirLowerError<'db>;

    fn cx(&self) -> DbInterner<'db> {
        self.infcx.interner
    }

    fn try_fold_binder<T: TypeFoldable<DbInterner<'db>>>(
        &mut self,
        t: Binder<'db, T>,
    ) -> Result<Binder<'db, T>, Self::Error> {
        self.universes.push(None);
        let t = t.try_super_fold_with(self);
        self.universes.pop();
        t
    }

    fn try_fold_ty(&mut self, ty: Ty<'db>) -> Result<Ty<'db>, Self::Error> {
        if self.instantiating {
            if !ty.has_type_flags(TypeFlags::HAS_PARAM) {
                return Ok(ty);
            }
            let TyKind::Param(param) = ty.kind() else { return ty.try_super_fold_with(self) };
            let arg = self.subst.as_slice().get(param.index as usize).and_then(|arg| arg.ty());
            return arg.ok_or_else(|| {
                MirLowerError::GenericArgNotProvided(param.id.into(), self.subst.store())
            });
        }
        if !ty.has_type_flags(TypeFlags::HAS_ALIAS) {
            return Ok(ty);
        }

        match ty.kind() {
            TyKind::Alias(_, alias) => {
                let limit = self.infcx.interner.recursion_limit();
                if self.depth >= limit {
                    // A type's `Debug` reads the attached database. The alias is as deep as
                    // the limit, so only its start is kept.
                    let mut shown = hir_ty::attach_db(self.infcx.interner.db, || {
                        format!("{:?} {ty:?}", alias.kind) // alloc: error path
                    });
                    if let Some((cut, _)) = shown.char_indices().nth(2000) {
                        shown.truncate(cut);
                        shown.push_str("...");
                    }
                    return Err(MirLowerError::NotSupported(format!( // alloc: error path
                        "E0275: overflow evaluating the requirement `{shown} == _` (the \
                         recursion limit, {limit}, reached normalizing it)"
                    )));
                }
                self.depth += 1;
                let ty = stdx::ensure_sufficient_stack(|| self.normalize_alias(ty));
                self.depth -= 1;
                ty
            }
            // An argument can be the `impl Future` of an `async fn`, which the body of the
            // caller only knows by that name: an alias, normalized as the rest.
            _ => ty.try_super_fold_with(self),
        }
    }

    fn try_fold_const(&mut self, ct: Const<'db>) -> Result<Const<'db>, Self::Error> {
        let ConstKind::Param(param) = ct.kind() else {
            return ct.try_super_fold_with(self);
        };
        if !self.instantiating {
            return Ok(ct);
        }
        self.subst.as_slice().get(param.index as usize).and_then(|arg| arg.konst()).ok_or_else(
            || MirLowerError::GenericArgNotProvided(param.id.into(), self.subst.store()),
        )
    }

    fn try_fold_region(&mut self, region: Region<'db>) -> Result<Region<'db>, Self::Error> {
        let RegionKind::ReEarlyParam(param) = region.kind() else {
            return Ok(region);
        };
        if !self.instantiating {
            return Ok(region);
        }
        // A function generic over nothing but lifetimes is monomorphized without arguments,
        // and what its lifetimes are makes no difference to the code.
        let region = self.subst.as_slice().get(param.index as usize).and_then(|arg| arg.region());
        Ok(region.unwrap_or_else(|| Region::new_erased(self.infcx.interner)))
    }
}

impl<'db> Filler<'db> {
    /// The alias `ty`, instantiated, with its arguments normalized, normalized, and what it
    /// normalized to normalized.
    fn normalize_alias(&mut self, ty: Ty<'db>) -> Result<Ty<'db>, MirLowerError<'db>> {
        let ty = ty.try_super_fold_with(self)?;
        let ty = self.impl_opaque(ty);
        if let Some(hidden) = self.hidden_with_opaques(ty) {
            return self.try_fold_ty(hidden);
        }

        let ty = self.normalized(ty)?;
        // Normalization could introduce infer vars (for example, if the alias cannot be normalized),
        // and we must not have infer vars in the body.
        let ty = ty.replace_infer_with_error(self.infcx.interner);
        ty.try_super_fold_with(self)
    }

    /// `alias` as what it is an alias of. One that names the lifetimes of a binder around it,
    /// like `D::Data<'a>` in `for<'a> fn(&'a mut T) -> D::Data<'a>`, is normalized with
    /// placeholders in their place, which are put back afterwards.
    fn normalized(&mut self, alias: Ty<'db>) -> Result<Ty<'db>, MirLowerError<'db>> {
        let normalize = |ty| {
            ObligationCtxt::new(&self.infcx)
                .structurally_normalize_ty(&ObligationCause::dummy(), self.trait_env.param_env, ty)
                // alloc: error path
                .map_err(|_| MirLowerError::NotSupported(format!("can't normalize alias {ty:?}")))
        };
        if !alias.has_escaping_bound_vars() {
            return normalize(alias);
        }
        let (alias, regions, types, consts) =
            BoundVarReplacer::replace_bound_vars(&self.infcx, &mut self.universes, alias);
        let ty = normalize(alias)?;
        Ok(PlaceholderReplacer::replace_placeholders(
            &self.infcx,
            regions,
            types,
            consts,
            &self.universes,
            ty,
        ))
    }

    fn new(db: &'db Db, env: ParamEnvAndCrate<'db>, subst: GenericArgs<'db>) -> Self {
        let interner = DbInterner::new_with(db, env.krate);
        let infcx = interner.infer_ctxt().build(TypingMode::PostAnalysis);
        // alloc: none until a binder is entered; the solver's replacer takes a `&mut Vec`
        Self { infcx, trait_env: env, subst, universes: Vec::new(), depth: 0, instantiating: false }
    }

    /// The `impl Trait` a trait method returns is the one of the impl's method once the
    /// implementing type is known.
    fn impl_opaque(&self, ty: Ty<'db>) -> Ty<'db> {
        let TyKind::Alias(_, alias) = ty.kind() else { return ty };
        let AliasTyKind::Opaque { def_id } = alias.kind else { return ty };
        let SolverDefId::InternedOpaqueTyId(opaque) = def_id.into() else { return ty };
        opaque_of_impl(self.infcx.interner.db, self.trait_env, opaque, alias.args).unwrap_or(ty)
    }

    /// The hidden type of the `impl Trait` a function returns, when another `impl Trait` is
    /// in it. One of a trait method (`fn f<T: A>(t: T) -> impl X { t.f() }`, or the body of
    /// `impl<T: A> B for T`) is the impl's once the types are known, which only
    /// [`Self::impl_opaque`] finds: the solver reveals all of it at once and stops at the
    /// trait method's, which has no code.
    fn hidden_with_opaques(&self, ty: Ty<'db>) -> Option<Ty<'db>> {
        let TyKind::Alias(_, alias) = ty.kind() else { return None };
        let AliasTyKind::Opaque { def_id } = alias.kind else { return None };
        let SolverDefId::InternedOpaqueTyId(opaque) = def_id.into() else { return None };
        let db = self.infcx.interner.db;
        let crate::ImplTraitId::ReturnTypeImplTrait(func, idx) = opaque.loc(db) else {
            return None;
        };
        // A trait's own is the impl's, which `impl_opaque` has not found, except of a provided
        // method the impl leaves as the trait has it: its body is the trait's, and the solver
        // reveals nothing of the trait's.
        let in_trait = matches!(
            hir_def::Lookup::lookup(&func, db).container,
            hir_def::ItemContainerId::TraitId(_)
        );
        if in_trait && !hir_def::signatures::FunctionSignature::of(db, func).has_body() {
            return None;
        }
        if alias.args.has_type_flags(TypeFlags::HAS_PARAM) || references_non_lt_error(&alias.args) {
            return None;
        }
        let hidden = crate::rpit_hidden_types(db, func).get(idx)?;
        let hidden = hidden.get().instantiate(self.infcx.interner, alias.args).skip_norm_wip();
        (in_trait || hidden.has_type_flags(TypeFlags::HAS_TY_OPAQUE)).then_some(hidden)
    }

    /// `t` with the arguments in place of its parameters and its aliases normalized: how every
    /// type, constant and argument list of a body is filled, and one alone
    /// ([`monomorphized_ty`] and the rest).
    fn fill<T>(&mut self, t: T) -> Result<T, MirLowerError<'db>>
    where
        T: TypeFoldable<DbInterner<'db>> + TypeVisitableExt<DbInterner<'db>> + std::fmt::Debug,
    {
        // An alias rigid where the body was checked may normalize once its parameters are
        // filled in `PostAnalysis`, as rustc's `post_analysis_normalize` resets them.
        self.instantiating = true;
        let t = t.try_fold_with(self)?;
        self.instantiating = false;
        let t = rustc_type_ir::set_aliases_to_non_rigid(self.infcx.interner, t).skip_normalization();
        // Can't deep normalized as that'll try to normalize consts and fail.
        let t = t.try_fold_with(self)?;
        if references_non_lt_error(&t) {
            // alloc: error path
            Err(MirLowerError::NotSupported(format!("monomorphization resulted in errors: {t:?}")))
        } else {
            Ok(t)
        }
    }

    fn fill_ty(&mut self, t: &mut StoredTy) -> Result<(), MirLowerError<'db>> {
        *t = self.fill(t.as_ref())?.store();
        Ok(())
    }

    fn fill_const(&mut self, t: &mut StoredConst) -> Result<(), MirLowerError<'db>> {
        *t = self.fill(t.as_ref())?.store();
        Ok(())
    }

    fn fill_args(&mut self, t: &mut StoredGenericArgs) -> Result<(), MirLowerError<'db>> {
        *t = self.fill(t.as_ref())?.store();
        Ok(())
    }

    fn fill_operand(&mut self, op: &mut Operand) -> Result<(), MirLowerError<'db>> {
        match &mut op.kind {
            OperandKind::Constant { konst, ty } => {
                self.fill_const(konst)?;
                self.fill_ty(ty)?;
            }
            OperandKind::Allocation { allocation } => {
                let alloc = allocation.as_ref();
                let ty = self.fill(alloc.ty)?;
                if ty == alloc.ty {
                    return Ok(());
                }
                *allocation = Allocation::new(AllocationData {
                    ty,
                    // clone: a constant whose type changed, interned once per instance
                    memory: alloc.memory.clone(),
                    // FIXME: Do we need to fill the memory map too?
                    // clone: its memory map, with it
                    memory_map: alloc.memory_map.clone(),
                })
                .store();
            }
            OperandKind::Copy(_) | OperandKind::Move(_) | OperandKind::Static(_) => (),
        }
        Ok(())
    }

    fn fill_body(&mut self, body: &mut MirBody<'db>) -> Result<(), MirLowerError<'db>> {
        for (_, l) in body.locals.iter_mut() {
            self.fill_ty(&mut l.ty)?;
        }
        for callee in &mut body.inlined.callees {
            self.fill_args(&mut callee.args)?;
        }
        for (_, bb) in body.basic_blocks.iter_mut() {
            for statement in &mut bb.statements {
                match &mut statement.kind {
                    StatementKind::Assign(_, r) => match r {
                        Rvalue::Aggregate(ak, ops) => {
                            for op in &mut **ops {
                                self.fill_operand(op)?;
                            }
                            match ak {
                                super::AggregateKind::Array(ty)
                                | super::AggregateKind::Tuple(ty)
                                | super::AggregateKind::Closure(ty) => self.fill_ty(ty)?,
                                super::AggregateKind::Adt(_, subst) => self.fill_args(subst)?,
                                super::AggregateKind::Union(_, _) => (),
                            }
                        }
                        Rvalue::Use(op) => {
                            self.fill_operand(op)?;
                        }
                        Rvalue::Repeat(op, len) => {
                            self.fill_operand(op)?;
                            self.fill_const(len)?;
                        }
                        Rvalue::Cast(_, op, ty) => {
                            self.fill_operand(op)?;
                            self.fill_ty(ty)?;
                        }
                        Rvalue::CheckedBinaryOp(_, lhs, rhs) => {
                            self.fill_operand(lhs)?;
                            self.fill_operand(rhs)?;
                        }
                        Rvalue::UnaryOp(_, op) => {
                            self.fill_operand(op)?;
                        }
                        Rvalue::Ref(_, _)
                        | Rvalue::Len(_)
                        | Rvalue::Discriminant(_)
                        | Rvalue::CopyForDeref(_) => (),
                        Rvalue::BinaryOp(_, lhs, rhs) => {
                            self.fill_operand(lhs)?;
                            self.fill_operand(rhs)?;
                        }
                        Rvalue::ThreadLocalRef(n) | Rvalue::AddressOf(n) | Rvalue::NullaryOp(n) => {
                            match *n {}
                        }
                    },
                    StatementKind::InlineAsm(asm) => {
                        for operand in &mut asm.operands {
                            match operand {
                                super::AsmOperand::Reg { input: Some(value), .. }
                                | super::AsmOperand::Const(value)
                                | super::AsmOperand::Sym(value) => self.fill_operand(value)?,
                                super::AsmOperand::Reg { input: None, .. }
                                | super::AsmOperand::Label => (),
                            }
                        }
                    }
                    StatementKind::Deinit(_)
                    | StatementKind::FakeRead(_)
                    | StatementKind::StorageLive(_)
                    | StatementKind::StorageDead(_)
                    | StatementKind::Nop => (),
                }
            }
            if let Some(terminator) = &mut bb.terminator {
                match &mut terminator.kind {
                    TerminatorKind::Call { func, args, .. }
                    | TerminatorKind::TailCall { func, args, .. } => {
                        self.fill_operand(func)?;
                        for op in &mut **args {
                            self.fill_operand(op)?;
                        }
                    }
                    TerminatorKind::SwitchInt { discr, .. } => {
                        self.fill_operand(discr)?;
                    }
                    TerminatorKind::Assert { cond, msg, .. } => {
                        self.fill_operand(cond)?;
                        match msg {
                            super::AssertKind::BoundsCheck { len: a, index: b }
                            | super::AssertKind::Overflow(_, a, b)
                            | super::AssertKind::MisalignedPointerDereference {
                                required: a,
                                found: b,
                            } => {
                                self.fill_operand(a)?;
                                self.fill_operand(b)?;
                            }
                            super::AssertKind::OverflowNeg(a)
                            | super::AssertKind::DivisionByZero(a)
                            | super::AssertKind::RemainderByZero(a)
                            | super::AssertKind::InvalidEnumConstruction(a) => self.fill_operand(a)?,
                            super::AssertKind::NullPointerDereference
                            | super::AssertKind::NullReferenceConstructed => (),
                        }
                    }
                    TerminatorKind::Goto { .. }
                    | TerminatorKind::UnwindResume
                    | TerminatorKind::Abort
                    | TerminatorKind::Return
                    | TerminatorKind::Unreachable
                    | TerminatorKind::Drop { .. }
                    | TerminatorKind::DropAndReplace { .. }
                    | TerminatorKind::Yield { .. }
                    | TerminatorKind::CoroutineDrop
                    | TerminatorKind::FalseEdge { .. }
                    | TerminatorKind::FalseUnwind { .. } => (),
                }
            }
        }
        Ok(())
    }
}

/// `body` with `subst` in place of its generic parameters. The queries below keep the result;
/// a caller that needs it once asks here instead.
pub fn monomorphized<'db>(
    db: &'db Db,
    body: &MirBody<'db>,
    subst: &StoredGenericArgs,
    trait_env: &StoredParamEnvAndCrate,
) -> Result<MirBody<'db>, MirLowerError<'db>> {
    let mut filler = Filler::new(db, trait_env.as_ref(db), subst.as_ref());
    // clone: the copy the query keeps; the fold changes it in place
    let mut body = body.clone();
    filler.fill_body(&mut body)?;
    Ok(body)
}

/// `ty` as [`monomorphized`] fills it in a body with `subst` and `trait_env`: one value, by the
/// same fold, so the two agree by construction.
pub fn monomorphized_ty<'db>(
    db: &'db Db,
    ty: Ty<'db>,
    subst: &StoredGenericArgs,
    trait_env: &StoredParamEnvAndCrate,
) -> Result<Ty<'db>, MirLowerError<'db>> {
    Filler::new(db, trait_env.as_ref(db), subst.as_ref()).fill(ty)
}

/// `konst` as [`monomorphized`] fills it in a body, as [`monomorphized_ty`] a type.
pub fn monomorphized_const<'db>(
    db: &'db Db,
    konst: Const<'db>,
    subst: &StoredGenericArgs,
    trait_env: &StoredParamEnvAndCrate,
) -> Result<Const<'db>, MirLowerError<'db>> {
    Filler::new(db, trait_env.as_ref(db), subst.as_ref()).fill(konst)
}

/// `args` as [`monomorphized`] fills a list of them in a body, as [`monomorphized_ty`] a type.
pub fn monomorphized_args<'db>(
    db: &'db Db,
    args: GenericArgs<'db>,
    subst: &StoredGenericArgs,
    trait_env: &StoredParamEnvAndCrate,
) -> Result<GenericArgs<'db>, MirLowerError<'db>> {
    Filler::new(db, trait_env.as_ref(db), subst.as_ref()).fill(args)
}

/// The type of the impl's method that `opaque`, the `impl Trait` a trait's method returns,
/// is with `args`, once they name the impl: `None` for one of another function, or while they
/// don't.
pub fn opaque_of_impl<'db>(
    db: &'db Db,
    env: ParamEnvAndCrate<'db>,
    opaque: InternedOpaqueTyId<'db>,
    args: GenericArgs<'db>,
) -> Option<Ty<'db>> {
    let crate::ImplTraitId::ReturnTypeImplTrait(func, idx) = opaque.loc(db) else { return None };
    if !matches!(hir_def::Lookup::lookup(&func, db).container, hir_def::ItemContainerId::TraitId(_))
    {
        return None;
    }
    // A lifetime nothing settled is an error one, and no code depends on it.
    let unsettled = TypeFlags::HAS_PARAM | TypeFlags::HAS_TY_INFER | TypeFlags::HAS_CT_INFER;
    if args.has_type_flags(unsettled) || references_non_lt_error(&args) {
        return None;
    }
    let interner = DbInterner::new_with(db, env.krate);
    // The arguments for the lifetimes the method's parameters leave out follow its own.
    let elided = crate::elided_lifetimes(db, func).len() as usize;
    let own = args.len().checked_sub(elided)?;
    let args_for_bounds = args.as_slice();
    let (own, _) = args_for_bounds.split_at(own);
    // Which impl it is does not depend on lifetimes, and those of a solver's own are not ours.
    let own = rustc_type_ir::fold_regions(interner, GenericArgs::new_from_slice(own), |_, _| {
        Region::new_erased(interner)
    });
    let (either::Either::Left(implemented), args) = db.lookup_impl_method(env, func, own) else {
        return None;
    };
    if implemented == func {
        return None;
    }
    let output = |method: hir_def::FunctionId, args: GenericArgs<'db>| {
        db.callable_item_signature(method.into())
            .instantiate(interner, args)
            .skip_norm_wip()
            .skip_binder()
            .output()
    };
    let declared = output(func, own);
    let actual = rustc_type_ir::fold_regions(interner, output(implemented, args), |_, _| {
        Region::new_erased(interner)
    });
    let opaque_id = |ty: Ty<'db>| {
        let TyKind::Alias(_, alias) = ty.kind() else { return None };
        let AliasTyKind::Opaque { def_id } = alias.kind else { return None };
        let SolverDefId::InternedOpaqueTyId(id) = def_id.into() else { return None };
        Some(id)
    };
    if opaque_id(declared) == Some(opaque) {
        return Some(actual);
    }
    // Match the implementation's return type and associated type bindings with the
    // trait's. Its concrete types need not declare any opaque types in the same positions.
    let bounds = crate::SelfAndAssocBounds::return_type_impl_traits(db, func);
    let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
    // alloc: once per opaque type of a trait method, not per body
    let vars: Vec<_> = bounds.iter().map(|_| infcx.next_ty_var(crate::Span::Dummy)).collect();
    let mut folder = crate::next_solver::util::BottomUpFolder {
        interner,
        ty_op: |ty| match opaque_id(ty).map(|id| id.loc(db)) {
            Some(crate::ImplTraitId::ReturnTypeImplTrait(f, i)) if f == func => {
                vars.get(i.into_raw().into_u32() as usize).copied().unwrap_or(ty)
            }
            _ => ty,
        },
        lt_op: |region| region,
        ct_op: |ct| ct,
    };
    let mut ocx = ObligationCtxt::new(&infcx);
    let cause = ObligationCause::dummy();
    let declared =
        rustc_type_ir::fold_regions(interner, declared, |_, _| Region::new_erased(interner));
    ocx.eq(&cause, env.param_env, declared.fold_with(&mut folder), actual).ok()?;
    for bound in bounds {
        for clause in bound.self_bounds().iter_instantiated_copied(interner, args_for_bounds) {
            if matches!(clause.kind().skip_binder(), rustc_type_ir::ClauseKind::Projection(_)) {
                let clause =
                    rustc_type_ir::fold_regions(interner, clause.skip_norm_wip(), |_, _| {
                        Region::new_erased(interner)
                    });
                ocx.register_obligation(crate::next_solver::infer::traits::Obligation::new(
                    interner,
                    // clone: an obligation's cause, once per bound of an opaque type
                    cause.clone(),
                    env.param_env,
                    clause.fold_with(&mut folder),
                ));
            }
        }
    }
    if !ocx.evaluate_obligations_error_on_ambiguity().is_empty() {
        return None;
    }
    let ty = infcx.deeply_resolve_ignoring_regions(*vars.get(idx.into_raw().into_u32() as usize)?);
    (!ty.has_infer() && !ty.has_escaping_bound_vars() && !references_non_lt_error(&ty))
        .then_some(ty)
}

/// `ty` of a monomorphized body with its aliases normalized, as the body's own types are.
pub(crate) fn normalized_ty<'db>(
    db: &'db Db,
    ty: Ty<'db>,
    trait_env: ParamEnvAndCrate<'db>,
) -> Result<Ty<'db>, MirLowerError<'db>> {
    ty.try_fold_with(&mut Filler::new(db, trait_env, GenericArgs::empty()))
}

/// `value` with every lifetime not bound in it erased and the variables of each binder in it
/// numbered in their order, unnamed: rustc's `erase_and_anonymize_regions`, how a vtable and a
/// `TypeId` have a type, so that two spellings of one type are one.
pub fn erase_and_anonymize_regions<'db, T: TypeFoldable<DbInterner<'db>>>(
    interner: DbInterner<'db>,
    value: T,
) -> T {
    if !value.has_type_flags(TypeFlags::HAS_BINDER_VARS | TypeFlags::HAS_FREE_REGIONS) {
        return value;
    }
    value.fold_with(&mut EraseAndAnonymize(interner))
}

struct EraseAndAnonymize<'db>(DbInterner<'db>);

impl<'db> TypeFolder<DbInterner<'db>> for EraseAndAnonymize<'db> {
    fn cx(&self) -> DbInterner<'db> {
        self.0
    }

    fn fold_binder<T: TypeFoldable<DbInterner<'db>>>(
        &mut self,
        binder: Binder<'db, T>,
    ) -> Binder<'db, T> {
        self.0.anonymize_bound_vars(binder).super_fold_with(self)
    }

    fn fold_region(&mut self, region: Region<'db>) -> Region<'db> {
        match region.kind() {
            RegionKind::ReBound(..) => region,
            _ => Region::new_erased(self.0),
        }
    }
}

/// The constant behind a promoted `&expr` of `owner`, as the instance with `subst` sees it.
#[blaze_query::tracked(returns(as_ref), lru = 0)]
pub fn monomorphized_promoted_mir_body_query<'db>(
    db: &'db Db,
    owner: InferBodyId<'db>,
    expr: hir_def::hir::ExprId,
    subst: StoredGenericArgs,
    trait_env: StoredParamEnvAndCrate,
) -> Result<MirBody<'db>, MirLowerError<'db>> {
    let body = super::runtime_promoted_mir_body_query(db, owner, expr).map_err(Clone::clone)?;
    monomorphized(db, body, &subst, &trait_env)
}
