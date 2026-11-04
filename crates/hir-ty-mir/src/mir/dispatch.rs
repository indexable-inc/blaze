//! A call's targets as the code generator dispatches them, one decision at a time: what a call
//! in a monomorphized body runs ([`call`], and past the intrinsics, drop glue and `Fn*` the
//! method it names, [`callee`]: the impl's method of a trait method with a concrete `Self`,
//! the `Future::poll` of an `async` block, a builtin derive's or `Clone`'s impl the compiler
//! supplies), what a drop runs ([`drop_glue`]), the parts a builtin `Clone::clone` clones
//! ([`clone_parts`]), the vtables an unsizing cast makes ([`unsized_vtables`]) and the methods
//! in their slots ([`vtable_methods`]).
//!
//! Each walk of the instances a body can run (may-suspend's, a build script's purity, the
//! inliner's) composes these, and the code generator's own `call_direct` is [`callee`], so what
//! a walk follows is what the compiled program runs.

use base_db::Db;
use blaze_hash::HashSet;
use blaze_query::db::{AsId, FromId};
use either::Either;
use hir_def::{
    AdtId, AssocItemId, BuiltinDeriveImplId, CallableDefId, FunctionId, GenericParamId,
    ItemContainerId, VariantId, builtin_derive::BuiltinDeriveImplMethod,
    signatures::FunctionSignature,
};
use hir_expand::name::Name;
use hir_ty::{
    InferBodyId, ParamEnvAndCrate,
    db::{HirDatabase, InternedClosureId, InternedCoroutineId},
    drop::{DropGlue, destructor, has_drop_glue},
    dyn_compatibility::generics_require_sized_self,
    intrinsic::{Intrinsic, UnknownIntrinsic, intrinsic_of},
    method_resolution::{NoImplMethod, is_dyn_method},
    next_solver::{
        DbInterner, GenericArg, GenericArgs, Region, RegionExt as _, StoredGenericArgs, TraitRef,
        Ty, TyKind, infer::InferCtxt,
    },
    traits::StoredParamEnvAndCrate,
};
use intern::sym;
use rustc_type_ir::{
    Interner as _,
    inherent::{BoundExistentialPredicates as _, Clause as _, GenericArgs as _, IntoKind as _},
};
use smallvec::SmallVec;
use std::alloc::Global;

use super::{MirBody, MirLowerError, is_by_move_coroutine, normalize_or_keep};

/// The most type nodes an instance's generic arguments may have: one past it is a type that
/// grows without end (polymorphic recursion), which a walk reports instead of following.
pub const LONGEST: usize = 256;

/// Whether `args` are within [`LONGEST`] type nodes.
pub fn within_longest(args: GenericArgs<'_>) -> bool {
    args.types().map(|it| it.walk().count()).sum::<usize>() <= LONGEST
}

/// The body an instance runs: the code generator's `InstanceDef` of those with MIR.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Body {
    Fn(FunctionId),
    /// An `InternedClosureId`, without its database lifetime.
    Closure(blaze_query::db::Id),
    /// An `InternedCoroutineId`: the `Future::poll` of an `async` block.
    Coroutine(blaze_query::db::Id),
    /// The same of the future an `async` closure gives when called by value.
    CoroutineByMove(blaze_query::db::Id),
}

/// The monomorphized runtime MIR of `body` with `args` (a closure's or coroutine's are its
/// parent's) in `env`.
pub fn body_of<'db>(
    db: &'db Db,
    env: StoredParamEnvAndCrate,
    body: Body,
    args: StoredGenericArgs,
) -> Result<&'db MirBody<'db>, MirLowerError<'db>> {
    match body {
        Body::Fn(func) => db.monomorphized_runtime_mir_body(InferBodyId::from(func), args, env),
        Body::Closure(id) => {
            db.monomorphized_runtime_mir_body_for_closure(InternedClosureId::from_id(id), args, env)
        }
        Body::Coroutine(id) => db.monomorphized_runtime_mir_body_for_coroutine(
            InternedCoroutineId::from_id(id),
            args,
            env,
        ),
        Body::CoroutineByMove(id) => db.monomorphized_runtime_mir_body_for_coroutine_by_move(
            InternedCoroutineId::from_id(id),
            args,
            env,
        ),
    }
}

/// The body that is the `poll` of the future `ty`, when that is one of an `async` block, and
/// its parent's arguments.
pub fn coroutine_poll<'db>(db: &'db Db, ty: Ty<'db>) -> Option<(Body, GenericArgs<'db>)> {
    let TyKind::Coroutine(id, args) = ty.kind() else { return None };
    let args = args.as_coroutine();
    let body = match is_by_move_coroutine(db, id.0, args) {
        true => Body::CoroutineByMove(id.0.as_id()),
        false => Body::Coroutine(id.0.as_id()),
    };
    Some((body, GenericArgs::new_from_slice(args.parent_args())))
}

/// What [`impl_method`] finds: a function, or the method of an impl a builtin derive makes.
pub type ImplMethod = Either<FunctionId, (BuiltinDeriveImplId, BuiltinDeriveImplMethod)>;

/// The method of the impl a call of `func` with `args` runs, or `func` itself where its impl
/// is chosen later or is the compiler's ([`NoImplMethod::is_expected`]); any other reason no
/// impl's method is found is the error.
pub fn impl_method<'db>(
    db: &'db Db,
    env: ParamEnvAndCrate<'db>,
    func: FunctionId,
    args: GenericArgs<'db>,
) -> Result<(ImplMethod, GenericArgs<'db>), NoImplMethod> {
    match db.lookup_impl_method_or_why(env, func, args) {
        Ok(found) => Ok(found),
        Err(why) if why.is_expected() => Ok((Either::Left(func), args)),
        Err(why) => Err(why),
    }
}

/// What a call of `func` runs past the intrinsics, drop glue and `Fn*`: the code generator's
/// `call_direct`.
#[derive(Clone, Copy, Debug)]
pub enum Callee<'db> {
    /// `Future::poll` of an `async` block: its body, with its parent's arguments.
    Poll(Body, GenericArgs<'db>),
    /// A trait method called on `dyn Trait`: the slot of the object's vtable.
    Virtual,
    /// The method of an impl a builtin derive makes, which a database that records builtin
    /// derives unexpanded has no body for.
    Derived(BuiltinDeriveImplId, BuiltinDeriveImplMethod),
    /// `Clone::clone` of a type whose impl the compiler supplies ([`clone_parts`]), with the
    /// arguments the lookup gave.
    CloneBuiltin(GenericArgs<'db>),
    /// A method of `FnPtr`, the compiler's: a function pointer is its address.
    FnPtr(GenericArgs<'db>),
    /// The function itself: the impl's method, or the trait's own where the impl is chosen
    /// later or is the compiler's. It may have no body (a foreign function, a trait method
    /// without a default), which is the caller's to refuse.
    Fn(FunctionId, GenericArgs<'db>),
}

/// What a call of `func` with `args` runs past the intrinsics, drop glue and `Fn*`, in the
/// caller's environment `env`; why no impl's method is found where that is not expected.
pub fn callee<'db>(
    infcx: &InferCtxt<'db>,
    env: ParamEnvAndCrate<'db>,
    func: FunctionId,
    args: GenericArgs<'db>,
) -> Result<Callee<'db>, NoImplMethod> {
    let interner = infcx.interner;
    let (db, lang) = (interner.db, interner.lang_items());
    if Some(func) == lang.FuturePoll
        && let Some((body, parent)) =
            coroutine_poll(db, normalize_or_keep(infcx, env.param_env, args.type_at(0)))
    {
        return Ok(Callee::Poll(body, parent));
    }
    if is_dyn_method(interner, env.param_env, func, args).is_some() {
        return Ok(Callee::Virtual);
    }
    let (found, found_args) = impl_method(db, env, func, args)?;
    let found = match found {
        Either::Left(found) => found,
        Either::Right((impl_, method)) => return Ok(Callee::Derived(impl_, method)),
    };
    if found == func
        && let ItemContainerId::TraitId(trait_) = func.loc(db).container
    {
        if Some(trait_) == lang.Clone && FunctionSignature::of(db, func).name.as_str() == "clone" {
            return Ok(Callee::CloneBuiltin(found_args));
        }
        if Some(trait_) == lang.FnPtrTrait {
            return Ok(Callee::FnPtr(found_args));
        }
    }
    Ok(Callee::Fn(found, found_args))
}

/// What a call of a function by name runs, one step: the intrinsics, drop glue and `Fn*` the
/// code generator lowers itself, then the method ([`callee`]).
#[derive(Clone, Debug)]
pub enum Call<'db> {
    /// `drop_in_place` or `drop_glue` of the type: its drop glue ([`drop_glue`]).
    DropGlue(Ty<'db>),
    /// `const_eval_select(args, const_fn, runtime_fn)`: at run time a call of `runtime_fn`
    /// with the fields of the tuple `args`; `None` where `runtime_fn` is not a function item.
    ConstEvalSelect(Option<(CallableDefId, GenericArgs<'db>)>),
    /// Any other intrinsic, whose code is the code generator's.
    Intrinsic(Intrinsic),
    /// A function marked an intrinsic that is not one known.
    UnknownIntrinsic(UnknownIntrinsic),
    /// `Fn*` of a closure: its body, with its parent's arguments, called with the closure
    /// and the fields of the arguments' tuple.
    Closure(Body, GenericArgs<'db>),
    /// `Fn*` of a function item: a call of it with the fields of the arguments' tuple.
    FnItem(CallableDefId, GenericArgs<'db>),
    /// `Fn*` through a function pointer or `dyn Fn*`: what it reaches was made where the
    /// pointer or vtable was.
    Indirect,
    /// A call of an `async` closure, through `AsyncFn*` or `Fn*`.
    AsyncClosure,
    /// A method, past all of the above.
    Method(Result<Callee<'db>, NoImplMethod>),
}

/// What a call of `func` with `args` runs, one step, as the code generator dispatches it.
pub fn call<'db>(
    infcx: &InferCtxt<'db>,
    env: ParamEnvAndCrate<'db>,
    func: FunctionId,
    args: GenericArgs<'db>,
) -> Call<'db> {
    let interner = infcx.interner;
    let (db, lang) = (interner.db, interner.lang_items());
    if Some(func) == lang.DropInPlace || Some(func) == lang.DropGlue {
        return Call::DropGlue(args.type_at(0));
    }
    match intrinsic_of(db, func) {
        Ok(Some(Intrinsic::ConstEvalSelect)) => {
            let runtime = match args.types().nth(2).map(|it| it.kind()) {
                Some(TyKind::FnDef(def, fn_args)) => Some((def.0, fn_args.skip_binder())),
                _ => None,
            };
            return Call::ConstEvalSelect(runtime);
        }
        Ok(Some(intrinsic)) => return Call::Intrinsic(intrinsic),
        Err(unknown) => return Call::UnknownIntrinsic(unknown),
        Ok(None) => {}
    }
    if let ItemContainerId::TraitId(trait_) = func.loc(db).container {
        if [lang.Fn, lang.FnMut, lang.FnOnce].contains(&Some(trait_)) {
            return fn_trait(infcx, env, func, args, args.type_at(0));
        }
        if [lang.AsyncFn, lang.AsyncFnMut, lang.AsyncFnOnce].contains(&Some(trait_)) {
            return Call::AsyncClosure;
        }
    }
    Call::Method(callee(infcx, env, func, args))
}

/// `Fn*::call*` for `Self` = `self_ty`.
fn fn_trait<'db>(
    infcx: &InferCtxt<'db>,
    env: ParamEnvAndCrate<'db>,
    func: FunctionId,
    args: GenericArgs<'db>,
    self_ty: Ty<'db>,
) -> Call<'db> {
    match normalize_or_keep(infcx, env.param_env, self_ty).kind() {
        TyKind::Ref(_, inner, _) => fn_trait(infcx, env, func, args, inner),
        TyKind::Closure(id, closure_args) => {
            let parent = GenericArgs::new_from_slice(closure_args.as_closure().parent_args());
            Call::Closure(Body::Closure(id.0.as_id()), parent)
        }
        TyKind::FnDef(def, fn_args) => Call::FnItem(def.0, fn_args.skip_binder()),
        TyKind::FnPtr(..) | TyKind::Dynamic(..) => Call::Indirect,
        TyKind::CoroutineClosure(..) => Call::AsyncClosure,
        _ => Call::Method(callee(infcx, env, func, args)),
    }
}

/// The parts `Clone::clone` of `ty` clones where the compiler supplies the impl: a tuple's
/// fields, a closure's captures and an array's element; none for the rest, which are `Copy`.
/// `None` for a closure whose captures are not known.
pub fn clone_parts<'db>(
    infcx: &InferCtxt<'db>,
    env: ParamEnvAndCrate<'db>,
    ty: Ty<'db>,
) -> Option<SmallVec<[Ty<'db>; 4]>> {
    let ty = normalize_or_keep(infcx, env.param_env, ty);
    Some(match ty.kind() {
        TyKind::Tuple(fields) => fields.iter().collect(), // alloc: inline up to four parts
        TyKind::Closure(..) | TyKind::CoroutineClosure(..) => {
            match ty.closure_upvars_ty().map(|it| it.kind()) {
                Some(TyKind::Tuple(fields)) => fields.iter().collect(), // alloc: as above
                _ => return None,
            }
        }
        TyKind::Array(element, _) => SmallVec::from_slice(&[element]),
        _ => SmallVec::new(),
    })
}

/// One thing the drop glue of a type runs, in the order it runs them.
#[derive(Clone, Copy, Debug)]
pub enum DropStep<'db> {
    /// `Drop::drop` (the function) of the type (the arguments): a call ([`call`]).
    Drop(FunctionId, GenericArgs<'db>),
    /// A type with a `Drop` impl, where the toolchain has no `Drop::drop`.
    NoDropTrait,
    /// A type whose drop glue is not followed here: a coroutine's, or one not known.
    Unknown(Ty<'db>),
}

/// The drop glue of `ty`: its `Drop` impl's `drop`, then a box's pointee's and its fields',
/// each type once in `seen`. A `dyn`'s is its vtable's, made where the vtable is.
pub fn drop_glue<'db>(
    infcx: &InferCtxt<'db>,
    env: ParamEnvAndCrate<'db>,
    ty: Ty<'db>,
    seen: &mut HashSet<Ty<'db>, Global>,
    out: &mut SmallVec<[DropStep<'db>; 4]>,
) {
    let interner = infcx.interner;
    let (db, lang) = (interner.db, interner.lang_items());
    let ty = normalize_or_keep(infcx, env.param_env, ty);
    if !seen.insert(ty) || has_drop_glue(infcx, ty, env.param_env) == DropGlue::None {
        return;
    }
    match ty.kind() {
        TyKind::Adt(def, args) => {
            let adt = def.def_id();
            if destructor(db, adt).is_some() {
                let drop = lang.Drop.and_then(|it| {
                    it.trait_items(db).method_by_name(&Name::new_symbol_root(sym::drop))
                });
                out.push(match drop {
                    Some(drop) => {
                        DropStep::Drop(drop, GenericArgs::new_from_slice(&[GenericArg::from(ty)]))
                    }
                    None => DropStep::NoDropTrait,
                });
            }
            // A box's pointee is dropped by its glue, not through its fields' (a pointer).
            if let AdtId::StructId(it) = adt
                && lang.OwnedBox == Some(it)
            {
                drop_glue(infcx, env, args.type_at(0), seen, out);
            }
            for variant in variants(db, adt) {
                for (_, field) in db.field_types(variant).iter() {
                    let field = field.ty().instantiate(interner, args).skip_norm_wip();
                    drop_glue(infcx, env, field, seen, out);
                }
            }
        }
        TyKind::Tuple(fields) => fields.iter().for_each(|it| drop_glue(infcx, env, it, seen, out)),
        TyKind::Array(element, _) | TyKind::Slice(element) => {
            drop_glue(infcx, env, element, seen, out)
        }
        TyKind::Closure(_, args) => {
            drop_glue(infcx, env, args.as_closure().tupled_upvars_ty(), seen, out)
        }
        TyKind::Dynamic(..) => {}
        _ => out.push(DropStep::Unknown(ty)),
    }
}

/// One thing an unsizing coercion makes, in the order it makes them.
#[derive(Clone, Copy, Debug)]
pub enum UnsizeStep<'db> {
    /// A vtable of the concrete type (the first) for the `dyn` type (the second).
    Vtable(Ty<'db>, Ty<'db>),
    /// An unsizing of a type that is not a struct.
    NotStruct(Ty<'db>),
    /// An unsizing of a struct without fields.
    NoFields(Ty<'db>),
    /// An unsizing of the first type to the second this does not follow.
    Unfollowed(Ty<'db>, Ty<'db>),
}

/// The vtables an unsizing coercion of `from` to `to` makes. An upcast of one `dyn` to another
/// makes none: the vtable it starts from was made where it was.
pub fn unsized_vtables<'db>(
    infcx: &InferCtxt<'db>,
    env: ParamEnvAndCrate<'db>,
    from: Ty<'db>,
    to: Ty<'db>,
    out: &mut SmallVec<[UnsizeStep<'db>; 2]>,
) {
    let interner = infcx.interner;
    let normalize = |ty| normalize_or_keep(infcx, env.param_env, ty);
    let (from, to) = (normalize(from), normalize(to));
    if from == to {
        return;
    }
    match (from.kind(), to.kind()) {
        (
            TyKind::RawPtr(from, _) | TyKind::Ref(_, from, _),
            TyKind::RawPtr(to, _) | TyKind::Ref(_, to, _),
        ) => unsized_pointee(infcx, env, from, to, out),
        (TyKind::Pat(from, _), TyKind::Pat(to, _)) => unsized_vtables(infcx, env, from, to, out),
        (TyKind::Adt(def, args), TyKind::Adt(to_def, to_args))
            if def.def_id() == to_def.def_id() =>
        {
            let AdtId::StructId(id) = def.def_id() else {
                return out.push(UnsizeStep::NotStruct(from));
            };
            for (_, field) in interner.db.field_types(id.into()).iter() {
                let from = field.ty().instantiate(interner, args).skip_norm_wip();
                let to = field.ty().instantiate(interner, to_args).skip_norm_wip();
                unsized_vtables(infcx, env, from, to, out);
            }
        }
        _ => out.push(UnsizeStep::Unfollowed(from, to)),
    }
}

fn unsized_pointee<'db>(
    infcx: &InferCtxt<'db>,
    env: ParamEnvAndCrate<'db>,
    from: Ty<'db>,
    to: Ty<'db>,
    out: &mut SmallVec<[UnsizeStep<'db>; 2]>,
) {
    let interner = infcx.interner;
    let normalize = |ty| normalize_or_keep(infcx, env.param_env, ty);
    let (from, to) = (normalize(from), normalize(to));
    if from == to {
        return;
    }
    match (from.kind(), to.kind()) {
        (TyKind::Dynamic(..), TyKind::Dynamic(..)) | (TyKind::Array(..), TyKind::Slice(_)) => {}
        (_, TyKind::Dynamic(..)) => out.push(UnsizeStep::Vtable(from, to)),
        (TyKind::Adt(def, args), TyKind::Adt(to_def, to_args))
            if def.def_id() == to_def.def_id() =>
        {
            let AdtId::StructId(id) = def.def_id() else {
                return out.push(UnsizeStep::NotStruct(from));
            };
            let fields = interner.db.field_types(id.into());
            let Some((_, last)) = fields.iter().last() else {
                return out.push(UnsizeStep::NoFields(from));
            };
            let from = last.ty().instantiate(interner, args).skip_norm_wip();
            let to = last.ty().instantiate(interner, to_args).skip_norm_wip();
            unsized_pointee(infcx, env, from, to, out);
        }
        _ => out.push(UnsizeStep::Unfollowed(from, to)),
    }
}

/// The trait methods in the slots of the vtable of `concrete` for `dyn_ty`, with their
/// arguments: every method of the principal trait and its supertraits that a trait object can
/// call, each supertrait's before the trait's own. A method with type or const parameters of
/// its own has no slot. The drop slot is [`drop_glue`]'s.
pub fn vtable_methods<'db>(
    interner: DbInterner<'db>,
    concrete: Ty<'db>,
    dyn_ty: Ty<'db>,
) -> SmallVec<[(FunctionId, GenericArgs<'db>); 8]> {
    let mut out = SmallVec::new();
    let TyKind::Dynamic(bounds, _) = dyn_ty.kind() else { return out };
    let Some(principal) = bounds.principal() else { return out };
    let trait_ref =
        interner.instantiate_bound_regions_with_erased(principal.with_self_ty(interner, concrete));
    trait_methods(interner, trait_ref, &mut HashSet::new_in(Global), &mut out);
    out
}

fn trait_methods<'db>(
    interner: DbInterner<'db>,
    trait_ref: TraitRef<'db>,
    seen: &mut HashSet<TraitRef<'db>, Global>,
    out: &mut SmallVec<[(FunctionId, GenericArgs<'db>); 8]>,
) {
    if !seen.insert(trait_ref) {
        return;
    }
    let db = interner.db;
    let supertraits: SmallVec<[TraitRef<'db>; 4]> = interner
        .explicit_super_clauses_of(trait_ref.def_id)
        .iter_instantiated(interner, trait_ref.args.as_slice())
        .filter_map(|clause| clause.skip_norm_wip().0.as_trait_clause())
        .map(|pred| interner.instantiate_bound_regions_with_erased(pred).trait_ref)
        .collect(); // alloc: inline up to four supertraits, all read before any is walked
    for supertrait in supertraits {
        trait_methods(interner, supertrait, seen, out);
    }
    let trait_items = trait_ref.def_id.0.trait_items(db);
    for &(_, item) in trait_items.items.iter() {
        let AssocItemId::FunctionId(func) = item else { continue };
        if generics_require_sized_self(db, func.into()) {
            continue;
        }
        let mut generic = false;
        let args =
            GenericArgs::fill_rest(interner, func.into(), trait_ref.args.iter(), |_, id, _| {
                generic |= !matches!(id, GenericParamId::LifetimeParamId(_));
                Region::new_erased(interner).into()
            });
        if !generic {
            out.push((func, args));
        }
    }
}

/// The variants of `adt`: its one for a struct or union.
pub fn variants(db: &Db, adt: AdtId) -> SmallVec<[VariantId; 4]> {
    match adt {
        AdtId::StructId(it) => SmallVec::from_slice(&[VariantId::StructId(it)]),
        AdtId::UnionId(it) => SmallVec::from_slice(&[VariantId::UnionId(it)]),
        AdtId::EnumId(it) => it
            .enum_variants(db)
            .variants
            .values()
            .map(|&(id, _)| VariantId::EnumVariantId(id))
            .collect(), // alloc: inline up to four variants
    }
}
