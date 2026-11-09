//! rustc's drop check of a type, `dropck_outlives`: what dropping a value of the type needs to
//! still be alive (`kinds`), and the types its walk stopped at past the crate's
//! `#![recursion_limit]` (`overflows`), which the borrow checker reports as E0320, "overflow
//! while adding drop-check rules".
//!
//! Ported from the pinned nightly (`16d1ffac1`):
//! `compiler/rustc_trait_selection/src/traits/query/dropck_outlives.rs`
//! (`trivial_dropck_outlives`, `compute_dropck_outlives_with_errors`,
//! `dtorck_constraint_for_ty_inner`), `compiler/rustc_traits/src/dropck_outlives.rs`
//! (`adt_dtorck_constraint`, `dedup_dtorck_constraint`) and `compiler/rustc_middle/src/ty/
//! util.rs` (`destructor_constraints`) (MIT or Apache-2.0).
//!
//! The depth is rustc's, and so is which program overflows:
//!
//! - The walk over a type counts one level per array, slice, pattern type, tuple field,
//!   closure or coroutine capture it steps into.
//! - An ADT is not stepped into. Its constraint ([`adt_dtorck_constraint`]) is computed once,
//!   over its fields at depth 0. What that leaves, parameters and aliases, is instantiated
//!   where the ADT is dropped and goes back on the stack one level deeper.
//! - So `Option<Option<..<T as Tr>::A..>>` 129 deep overflows at the default limit of 128,
//!   on `Option<<T as Tr>::A>`. 128 deep does not, because an alias left at the bottom goes
//!   to `kinds` and is never walked.
//! - A depth past the limit (`depth > limit`, rustc's `value_within_limit`) records the type
//!   as an overflow. The walk stops at the first type that overflowed and ends with what it
//!   has, as rustc's does.
//!
//! The limit is the crate's `#![recursion_limit]`, the one the solver gets
//! (`Interner::recursion_limit` of [`DbInterner`]). [`adt_dtorck_constraint`] is keyed by it as well as by
//! the ADT: rustc's query reads the limit of the crate being compiled.
//!
//! When a type left by an ADT does not normalize, the walk stops with the solver's errors
//! ([`DropckError`]), as rustc's does, which then reports them as fulfillment errors.

use base_db::Db;
use blaze_mem::Frame;
use hir_def::{AdtId, GenericDefId, HasModule, attrs::AttrFlags};
use rustc_type_ir::{
    EarlyBinder,
    inherent::{AdtDef as _, GenericArgs as _, IntoKind},
};

use crate::{
    consteval,
    db::HirDatabase,
    drop::{DropGlue, has_drop_glue},
    frame_set,
    next_solver::{
        ConstKind, DbInterner, GenericArg, GenericArgKind, GenericArgs, ParamEnv, RegionKind,
        StoredGenericArg, StoredTy, Ty, TyKind, TypingMode,
        fulfill::NextSolverError,
        infer::{DbInternerInferExt, InferCtxt, traits::ObligationCause},
        obligation_ctxt::ObligationCtxt,
    },
};

/// rustc's `DropckOutlivesResult`: what dropping a type needs live, and the types its walk
/// went past the recursion limit at (at most the first round's, as rustc stops there), in
/// the caller's frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropckOutlivesResult<'f, 'db> {
    pub kinds: Vec<GenericArg<'db>, &'f Frame>,
    pub overflows: Vec<Ty<'db>, &'f Frame>,
}

/// A type the drop check had to normalize and could not: the solver's errors, which rustc
/// reports as fulfillment errors where it reports the drop check's overflow.
#[derive(Debug, Clone)]
pub struct DropckError<'db> {
    pub ty: Ty<'db>,
    pub errors: Vec<NextSolverError<'db>>,
}

/// rustc's `DropckConstraint` of an ADT, generic in the ADT's own parameters: what its
/// destructor and fields need live (`outlives`), the types left to take apart where it is
/// dropped (`dtorck_types`), and what its own fields overflowed at.
#[derive(Debug, Clone, PartialEq, Eq, Default, blaze_query::Value)]
pub struct DtorckConstraint {
    pub outlives: Vec<StoredGenericArg>,
    pub dtorck_types: Vec<StoredTy>,
    pub overflows: Vec<StoredTy>,
}

/// [`DtorckConstraint`] while it is built, of one interner's types, in the walk's frame.
struct Constraint<'f, 'db> {
    outlives: Vec<GenericArg<'db>, &'f Frame>,
    dtorck_types: Vec<Ty<'db>, &'f Frame>,
    overflows: Vec<Ty<'db>, &'f Frame>,
}

impl<'f, 'db> Constraint<'f, 'db> {
    fn empty(frame: &'f Frame) -> Self {
        Constraint {
            outlives: Vec::new_in(frame),
            dtorck_types: Vec::new_in(frame),
            overflows: Vec::new_in(frame),
        }
    }
}

/// rustc's `trivial_dropck_outlives`: dropping a `ty` needs nothing to outlive it. Implies,
/// but is not implied by, no drop glue: `PhantomData<T>` has none and is not trivial.
pub fn trivial_dropck_outlives<'db>(interner: DbInterner<'db>, ty: Ty<'db>) -> bool {
    use rustc_type_ir::InferTy;
    match ty.kind() {
        TyKind::Infer(InferTy::FreshIntTy(_))
        | TyKind::Infer(InferTy::FreshFloatTy(_))
        | TyKind::Bool
        | TyKind::Int(_)
        | TyKind::Uint(_)
        | TyKind::Float(_)
        | TyKind::Never
        | TyKind::FnDef(..)
        | TyKind::FnPtr(..)
        | TyKind::Char
        | TyKind::CoroutineWitness(..)
        | TyKind::RawPtr(_, _)
        | TyKind::Ref(..)
        | TyKind::Str
        | TyKind::Foreign(..)
        | TyKind::Error(_) => true,
        TyKind::Pat(ty, _) | TyKind::Slice(ty) => trivial_dropck_outlives(interner, ty),
        // An empty array never has a destructor (rust-lang/rust#110288).
        TyKind::Array(ty, size) => match consteval::try_const_usize(interner.db, size) {
            Some(0) => true,
            _ => trivial_dropck_outlives(interner, ty),
        },
        TyKind::Tuple(tys) => tys.iter().all(|t| trivial_dropck_outlives(interner, t)),
        TyKind::Closure(_, args) => {
            trivial_dropck_outlives(interner, args.as_closure().tupled_upvars_ty())
        }
        TyKind::CoroutineClosure(_, args) => {
            trivial_dropck_outlives(interner, args.as_coroutine_closure().tupled_upvars_ty())
        }
        // `ManuallyDrop` never has a destructor. Every other ADT may, `PhantomData` owns its
        // content, and a union may have a `Drop` impl.
        TyKind::Adt(def, _) => def.is_manually_drop(),
        TyKind::Dynamic(..)
        | TyKind::Alias(..)
        | TyKind::Param(_)
        | TyKind::Placeholder(..)
        | TyKind::Infer(_)
        | TyKind::Bound(..)
        | TyKind::Coroutine(..)
        | TyKind::UnsafeBinder(_) => false,
    }
}

/// rustc's `dropck_outlives` of `dropped_ty` (the type op's fast path, then
/// `compute_dropck_outlives_with_errors`), with the recursion limit of `infcx`'s crate.
///
/// The types it normalizes go through `infcx`, whose inference state it changes: a caller
/// that wants none of it runs this in a probe. The walk and its result are in `frame`.
pub fn dropck_outlives<'f, 'db>(
    infcx: &InferCtxt<'db>,
    env: ParamEnv<'db>,
    dropped_ty: Ty<'db>,
    frame: &'f Frame,
) -> Result<DropckOutlivesResult<'f, 'db>, DropckError<'db>> {
    let interner = infcx.interner;
    let mut result =
        DropckOutlivesResult { kinds: Vec::new_in(frame), overflows: Vec::new_in(frame) };
    if trivial_dropck_outlives(interner, dropped_ty) {
        return Ok(result);
    }
    let walk = Walk {
        interner,
        limit: rustc_type_ir::Interner::recursion_limit(interner),
        needs_drop: &|ty| has_drop_glue(infcx, ty, env) != DropGlue::None,
    };
    {
        // Each type left to take apart, with its depth, and every type ever pushed: a type
        // that holds itself (`struct A { children: Vec<A> }`) is taken apart once.
        let mut ty_stack = Vec::new_in(frame);
        ty_stack.push((dropped_ty, 0usize));
        let mut ty_set = frame_set(frame);
        let cause = ObligationCause::dummy();
        let mut constraints = Constraint::empty(frame);
        while let Some((ty, depth)) = ty_stack.pop() {
            walk.constraint_for_ty(depth, ty, &mut constraints);
            result.kinds.append(&mut constraints.outlives);
            result.overflows.append(&mut constraints.overflows);
            // One overflow is enough: the ones after it say nothing new, and printing the
            // huge types they are at is slow.
            if !result.overflows.is_empty() {
                break;
            }
            for ty in constraints.dtorck_types.drain(..) {
                // rustc's `query_normalize`, which under the next solver is a deep
                // normalization that fails as a whole.
                let ocx = ObligationCtxt::new(infcx);
                let ty = match ocx.deeply_normalize(&cause, env, ty) {
                    Ok(ty) => ty,
                    Err(errors) => return Err(DropckError { ty, errors }),
                };
                match ty.kind() {
                    // Every parameter lives as long as the function.
                    TyKind::Param(_) => {}
                    // A projection that does not normalize may have a destructor.
                    TyKind::Alias(..) => result.kinds.push(ty.into()),
                    _ => {
                        if ty_set.insert(ty) {
                            ty_stack.push((ty, depth + 1));
                        }
                    }
                }
            }
        }
        Ok(result)
    }
}

/// The walk of one type: rustc's `dtorck_constraint_for_ty_inner` with what it reads of the
/// type context.
struct Walk<'a, 'db> {
    interner: DbInterner<'db>,
    limit: usize,
    /// rustc's `Ty::needs_drop` under the walk's typing environment, for what a coroutine
    /// holds across a suspension.
    needs_drop: &'a dyn Fn(Ty<'db>) -> bool,
}

impl<'db> Walk<'_, 'db> {
    /// rustc's `dtorck_constraint_for_ty_inner`: what dropping `ty`, reached at `depth`,
    /// needs, added to `constraints`. One level of Rust stack per level of `ty`, each with
    /// stack to spare (rule 20).
    fn constraint_for_ty(&self, depth: usize, ty: Ty<'db>, constraints: &mut Constraint<'_, 'db>) {
        stdx::ensure_sufficient_stack(|| self.constraint_for_ty_level(depth, ty, constraints))
    }

    fn constraint_for_ty_level(
        &self,
        depth: usize,
        ty: Ty<'db>,
        constraints: &mut Constraint<'_, 'db>,
    ) {
        // rustc's `Limit::value_within_limit`: `depth <= limit`.
        if depth > self.limit {
            constraints.overflows.push(ty);
            return;
        }
        let interner = self.interner;
        if trivial_dropck_outlives(interner, ty) {
            return;
        }
        match ty.kind() {
            TyKind::Bool
            | TyKind::Char
            | TyKind::Int(_)
            | TyKind::Uint(_)
            | TyKind::Float(_)
            | TyKind::Str
            | TyKind::Never
            | TyKind::Foreign(..)
            | TyKind::RawPtr(..)
            | TyKind::Ref(..)
            | TyKind::FnDef(..)
            | TyKind::FnPtr(..)
            | TyKind::CoroutineWitness(..) => {}
            // A container of one element type drops as its element does.
            TyKind::Pat(ety, _) | TyKind::Array(ety, _) | TyKind::Slice(ety) => {
                self.constraint_for_ty(depth + 1, ety, constraints)
            }
            TyKind::Tuple(tys) => {
                for ty in tys.iter() {
                    self.constraint_for_ty(depth + 1, ty, constraints);
                }
            }
            TyKind::Closure(_, args) => {
                for ty in args.as_closure().upvar_tys().iter() {
                    self.constraint_for_ty(depth + 1, ty, constraints);
                }
            }
            TyKind::CoroutineClosure(_, args) => {
                for ty in args.as_coroutine_closure().upvar_tys().iter() {
                    self.constraint_for_ty(depth + 1, ty, constraints);
                }
            }
            // rust-lang/rust#49918, #116242: when anything the coroutine holds across a
            // suspension needs dropping, every capture and the resume type must be live (as
            // if used), since a region in what it holds may have come from any of them. Else
            // only the captures are dropped.
            TyKind::Coroutine(def_id, args) => {
                let args = args.as_coroutine();
                let witnesses = interner.db.coroutine_witnesses(def_id.0);
                let needs_drop = witnesses.iter().any(|it| (self.needs_drop)(it.as_ref()));
                if needs_drop {
                    constraints.outlives.extend(args.upvar_tys().iter().map(GenericArg::from));
                    constraints.outlives.push(args.resume_ty().into());
                } else {
                    for ty in args.upvar_tys().iter() {
                        self.constraint_for_ty(depth + 1, ty, constraints);
                    }
                }
            }
            TyKind::Adt(def, args) => {
                let adt = adt_dtorck_constraint(interner.db, def.def_id(), self.limit as u32);
                let instantiate = |it: GenericArg<'db>| {
                    EarlyBinder::bind(interner, it).instantiate(interner, args).skip_norm_wip()
                };
                constraints.dtorck_types.extend(
                    adt.dtorck_types.iter().map(|it| instantiate(it.as_ref().into()).expect_ty()),
                );
                constraints.outlives.extend(adt.outlives.iter().map(|it| instantiate(it.as_ref())));
                constraints.overflows.extend(
                    adt.overflows.iter().map(|it| instantiate(it.as_ref().into()).expect_ty()),
                );
            }
            // A trait object must be alive for its destructor to be called.
            TyKind::Dynamic(..) => constraints.outlives.push(ty.into()),
            // Types that can't be taken apart here are passed on, as is a binder that can't be
            // instantiated here.
            TyKind::Alias(..) | TyKind::Param(..) | TyKind::UnsafeBinder(_) => {
                constraints.dtorck_types.push(ty)
            }
            // rustc's delayed bug: no inference variable or placeholder is left by then.
            TyKind::Placeholder(..) | TyKind::Bound(..) | TyKind::Infer(..) | TyKind::Error(_) => {}
        }
    }
}

/// rustc's `adt_dtorck_constraint` of `adt`, walked with `recursion_limit`: its fields, each
/// from depth 0, then what its `Drop` impl needs live ([`destructor_constraints`]), each list
/// without repeats.
#[blaze_query::tracked(returns(ref), cycle_result = adt_dtorck_constraint_cycle, mem_tag = Infer)]
pub fn adt_dtorck_constraint(db: &Db, adt: AdtId, recursion_limit: u32) -> DtorckConstraint {
    let krate = adt.module(db).krate(db);
    let interner = DbInterner::new_with(db, krate);
    let identity = GenericArgs::identity_for_item(interner, adt.into());
    let def = crate::next_solver::AdtDef::new(adt, interner);
    // `ManuallyDrop` is trivial: the walk never asks for it.
    if rustc_type_ir::inherent::AdtDef::is_phantom_data(def) {
        let Some(GenericArgKind::Type(it)) = identity.iter().next().map(|it| it.kind()) else {
            panic!("`PhantomData` ({adt:?}) has a type parameter first");
        };
        return DtorckConstraint {
            outlives: Vec::new(), // alloc: the query's value; `Vec::new` takes no memory
            dtorck_types: vec![it.store()], // alloc: the query's value, one type
            overflows: Vec::new(), // alloc: the query's value; `Vec::new` takes no memory
        };
    }
    let env = db.trait_environment(GenericDefId::AdtId(adt));
    let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
    let walk = Walk {
        interner,
        limit: recursion_limit as usize,
        needs_drop: &|ty| has_drop_glue(&infcx, ty, env) != DropGlue::None,
    };
    blaze_mem::frame::task(crate::db::INFER_TAG, |frame| {
        let mut result = Constraint::empty(frame);
        let mut fields = |variant: hir_def::VariantId| {
            for (_, field) in db.field_types(variant).iter() {
                let field = field.ty().instantiate_identity().skip_norm_wip();
                walk.constraint_for_ty(0, field, &mut result);
            }
        };
        match adt {
            AdtId::StructId(it) => fields(it.into()),
            AdtId::UnionId(it) => fields(it.into()),
            AdtId::EnumId(it) => {
                for &(variant, _) in it.enum_variants(db).variants.values() {
                    fields(variant.into());
                }
            }
        }
        result.outlives.extend(destructor_constraints(db, adt, identity));
        // rustc's `dedup_dtorck_constraint`: the first of each, in order.
        let mut seen = frame_set(frame);
        result.outlives.retain(|it| seen.insert(*it));
        let mut seen = frame_set(frame);
        result.dtorck_types.retain(|it| seen.insert(*it));
        DtorckConstraint {
            outlives: result.outlives.iter().map(|it| it.store()).collect(), // alloc: the query's value
            dtorck_types: result.dtorck_types.iter().map(|it| it.store()).collect(), // alloc: the query's value
            overflows: result.overflows.iter().map(|it| it.store()).collect(), // alloc: the query's value
        }
    })
}

/// An ADT that holds itself by value has no size, which is reported where its layout is
/// asked for; its drop check has nothing to add.
fn adt_dtorck_constraint_cycle(
    _db: &Db,
    _: blaze_query::db::Id,
    _adt: AdtId,
    _recursion_limit: u32,
) -> DtorckConstraint {
    DtorckConstraint::default()
}

/// rustc's `destructor_constraints`: of the ADT's own generic arguments `identity`, those its
/// `Drop` impl does not mark `#[may_dangle]`, which the destructor may use. None without a
/// `Drop` impl.
fn destructor_constraints<'db>(
    db: &'db Db,
    adt: AdtId,
    identity: GenericArgs<'db>,
) -> impl Iterator<Item = GenericArg<'db>> {
    let drop_impl = crate::drop::destructor(db, adt);
    let impl_args = drop_impl.and_then(|drop_impl| {
        match db.impl_self_ty(drop_impl).instantiate_identity().skip_norm_wip().kind() {
            TyKind::Adt(def, args) if def.def_id() == adt => Some(args),
            _ => None,
        }
    });
    let flags = drop_impl.map(|it| AttrFlags::query_generic_params(db, it.into()));
    identity
        .iter()
        .zip(impl_args.into_iter().flat_map(|it| it.iter()))
        .filter(move |&(_, arg)| {
            let Some((lifetimes, types)) = flags else { return false };
            let dangles =
                |it: Option<&AttrFlags>| it.is_some_and(|it| it.contains(AttrFlags::MAY_DANGLE));
            match arg.kind() {
                GenericArgKind::Lifetime(region) => match region.kind() {
                    RegionKind::ReEarlyParam(early) => !dangles(lifetimes.get(early.id.local_id)),
                    // Not a lifetime parameter: an error the impl's check reports.
                    _ => false,
                },
                GenericArgKind::Type(ty) => match ty.kind() {
                    TyKind::Param(param) => !dangles(types.get(param.id.local_id())),
                    _ => false,
                },
                GenericArgKind::Const(konst) => match konst.kind() {
                    ConstKind::Param(param) => !dangles(types.get(param.id.local_id())),
                    _ => false,
                },
            }
        })
        .map(|(item_arg, _)| item_arg)
}
