//! `Devirtualize`, blaze's own (rustc has none): a call through a `dyn` whose object type the
//! whole program makes vtables of for one to four concrete types, and no others can exist,
//! becomes a direct call of each type's method, behind a compare of the object's vtable
//! pointer where there are several (docs/v2/plan/lanes/codegen/devirt.md).
//!
//! What the program makes is the code generator's walk of every instance a final binary
//! reaches ([`hir_ty::mir::devirt::World`]), named by the body's [`MirOptions::world`]. Each
//! call the pass looks at pushes the digest of the facts it read to
//! `body.inlined.devirtualized`, which keys the body's code (rule 18): a body keyed by what it
//! read is built again when those facts change, and only then.
//!
//! Why the rewrite is the same program:
//! - A `dyn` call runs the method in the vtable the object's metadata points to. A vtable is
//!   made for a type `T` only by an unsizing of `T` to the object type, every one of which the
//!   walk lists (the Rust Reference, "Behavior considered undefined": a wide pointer's
//!   metadata must be "a vtable for `Trait` that matches the actual dynamic trait the pointer
//!   or reference points to", so no other metadata can reach the call in a program without
//!   UB; and pointer casts between trait objects need equal principal traits and arguments,
//!   rustc PR #120248, Rust 1.81). Where something can make an object the walk can't see (a
//!   loader, a foreign function, a `cdylib`'s caller, an upcast that loads a vtable out of
//!   another) the object is open and only a profile-speculated compare is made, with the
//!   `dyn` call kept behind it.
//! - The code generator makes one vtable object per type and object key in a whole build
//!   (`hir-ty-codegen` `vtable`), so the metadata of an object of `T` equals the pointer an
//!   unsizing of a `*const T` gives: the compare is exact, never a guess.
//! - The method each target runs is the one its vtable slot holds: the trait's method with
//!   `Self` the target, which the code generator resolves as it resolves the slot
//!   (`dispatch::callee`).
//!
//! It runs at `Release` with inlining and a world, once before `Inline` (so a direct call can
//! be inlined) and once after (a `dyn` call an inlined callee brought, `Box<dyn Fn>`'s
//! `Fn::call` among them). Only a call whose types name no parameter of the generic body is
//! looked at: the facts are of concrete object types.

use blaze_mem::Frame;
use hir_def::{CallableDefId, FunctionId, ItemContainerId};
use hir_ty::{
    GenericPredicates,
    method_resolution::is_dyn_method,
    mir::devirt::{self, ABSENT, Object, Target, World},
    next_solver::{
        DbInterner, GenericArg, GenericArgs, Region, RegionExt as _, Ty, TyKind,
        infer::traits::{Obligation, ObligationCause},
    },
};
use rustc_ast_ir::Mutability;
use rustc_type_ir::{
    TypeVisitableExt as _,
    inherent::{BoundExistentialPredicates as _, IntoKind as _, Ty as _},
};

use crate::mir::{
    BasicBlock, BasicBlockId, BinOp, CastKind, Local, LocalId, MirBody, MirLowerError, Operand, OperandKind, Place, ProjectionElem, Rvalue, Statement, StatementKind, SwitchTargets,
    Terminator, TerminatorKind, transform::Ctx,
};
use crate::{db::HirDatabase as _, infer::PointerCast};

use super::{Result, block_index};

/// The most concrete types a closed object's call is turned into compares for: past it the
/// compares cost more than the indirect call they replace.
const MAX_TARGETS: usize = 4;

/// Runs `Devirtualize` over `body` against the world `id` names. `speculate`: an open object
/// with a profile gets its hottest type compared first, the `dyn` call kept behind it; only in
/// the last run. `behind` holds the receiver locals of the `dyn` calls an earlier run kept
/// behind its compares, which a later run leaves as they are (`Inline` and `SimplifyCfg`
/// renumber blocks, never the caller's locals).
pub(super) fn devirtualize<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    id: devirt::DevirtWorld,
    speculate: bool,
    behind: &mut Vec<LocalId>,
    _frame: &Frame,
) -> Result<'db, ()> {
    let Some(world) = devirt::given(id) else {
        return Err(MirLowerError::ImplementationError(format!(
            "the MIR of {:?} is asked for devirtualized against the dyn facts {id:?}, which no \
             build holds (`hir_ty::mir::devirt::give`)",
            body.owner
        )));
    };
    let _span = tracing::info_span!(
        target: "blaze",
        "devirtualize",
        body = ?body.owner,
        world = ?id,
        program = %world.program,
        speculate,
    )
    .entered();
    // Block order, the order `inlined.devirtualized` records in: new blocks are allocated past
    // the ones looked at.
    let blocks = body.basic_blocks.len();
    for at in 0..blocks {
        let block = BasicBlockId::from_raw(la_arena::RawIdx::from_u32(at as u32));
        let Some(site) = site(ctx, body, block, behind) else { continue };
        let Some(object) = world.object(&site.key) else {
            body.inlined.devirtualized.push(ABSENT.0);
            tracing::info!(
                target: "blaze",
                block = block_index(block),
                outcome = "kept",
                reason = "the program makes no vtable of the object type: the call never runs",
            );
            continue;
        };
        body.inlined.devirtualized.push(object.digest.0);
        match plan(ctx, &site, object, &world, speculate) {
            Plan::Keep(reason) => tracing::info!(
                target: "blaze",
                block = block_index(block),
                object = %object.name,
                outcome = "kept",
                reason = %reason,
            ),
            Plan::Rewrite { guarded, last, outcome } => {
                tracing::info!(
                    target: "blaze",
                    block = block_index(block),
                    object = %object.name,
                    outcome,
                    targets = guarded.len() + usize::from(last.is_some()),
                );
                if let Some(receiver) = rewrite(ctx, body, block, &site, &guarded, last)? {
                    behind.push(receiver);
                }
            }
        }
    }
    Ok(())
}

/// A call the pass may turn direct: of a trait method through a `dyn` whose object type names
/// no parameter, its receiver a reference to the object.
struct Site<'db> {
    method: FunctionId,
    args: GenericArgs<'db>,
    dyn_ty: Ty<'db>,
    receiver: Mutability,
    key: devirt::ObjectKey,
}

fn site<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &MirBody<'db>,
    block: BasicBlockId,
    behind: &[LocalId],
) -> Option<Site<'db>> {
    let Terminator { kind: TerminatorKind::Call { func, args, intrinsic: None, .. }, .. } =
        body.basic_blocks[block].terminator.as_ref()?
    else {
        return None;
    };
    if let Some(Operand { kind: OperandKind::Copy(place), .. }) = args.first()
        && place.projection.is_empty()
        && behind.contains(&place.local)
    {
        return None;
    }
    let (CallableDefId::FunctionId(method), fn_args) = func.fn_def()? else { return None };
    let ItemContainerId::TraitId(_) = method.loc(ctx.db).container else { return None };
    let interner = ctx.infcx.interner;
    let dyn_ty = fn_args.as_slice().first()?.ty()?;
    if !matches!(dyn_ty.kind(), TyKind::Dynamic(..)) {
        return None;
    }
    is_dyn_method(interner, ctx.env, method, fn_args)?;
    let why = if fn_args.has_param() {
        Some("its object type names a parameter of the generic body")
    } else if fn_args.has_opaque_types() {
        Some("its object type names an opaque type the body does not see through")
    } else if GenericPredicates::query_own_explicit(ctx.db, method.into()).skip_binder().next().is_some()
    {
        Some("the method has where clauses of its own, which a type's method may not meet")
    } else {
        None
    };
    if let Some(why) = why {
        tracing::info!(target: "blaze", block = block_index(block), outcome = "kept", reason = why);
        return None;
    }
    let receiver = match args.first().map(|it| ctx.normalize(ctx.operand_ty(body, it)).kind()) {
        Some(TyKind::Ref(_, pointee, mutbl)) if erased(interner, pointee) == erased(interner, dyn_ty) => {
            mutbl
        }
        _ => {
            tracing::info!(
                target: "blaze",
                block = block_index(block),
                outcome = "kept",
                reason = "its receiver is not a reference to the object (`Box`, `Rc`, `Pin`)",
            );
            return None;
        }
    };
    let principal = devirt::principal_of(interner, dyn_ty, dyn_ty)?;
    let key = devirt::object_key(&ctx.infcx, ctx.env, principal);
    Some(Site { method, args: fn_args, dyn_ty, receiver, key })
}

fn erased<'db>(interner: DbInterner<'db>, ty: Ty<'db>) -> Ty<'db> {
    rustc_type_ir::fold_regions(interner, ty, |_, _| Region::new_erased(interner))
}

enum Plan<'db> {
    Keep(String),
    /// Compare the vtable against each of `guarded` in order and call its method on a match;
    /// past them call `last`'s method, or without one the `dyn` call. `outcome` names it for
    /// the trace: `direct`, `compared` or `speculated`.
    Rewrite { guarded: Vec<Ty<'db>>, last: Option<Ty<'db>>, outcome: &'static str },
}

fn plan<'db>(
    ctx: &Ctx<'_, 'db>,
    site: &Site<'db>,
    object: &Object,
    world: &World,
    speculate: bool,
) -> Plan<'db> {
    let keep = Plan::Keep;
    // The targets this `dyn` type can be: its projection bounds (`Item = u32`) are not part of
    // the vtable's key, so a type whose `Item` differs is not one of this call's. One the
    // solver can't decide keeps the call: dropping a type the object may be would be wrong.
    let mut targets: Vec<&Target> = Vec::with_capacity(object.targets.len()); // alloc: a handful a call
    for target in object.targets.iter() {
        match bounds_hold(ctx, site.dyn_ty, target.ty.as_ref()) {
            Some(true) => targets.push(target),
            Some(false) => {}
            None => {
                return keep(format!(
                    "can't tell whether {:?} meets the projection bounds of {}",
                    target.ty.as_ref(),
                    object.name
                ));
            }
        }
    }
    order(&mut targets);
    let tys = |it: &[&Target]| it.iter().map(|t| t.ty.as_ref()).collect::<Vec<_>>(); // alloc: a handful a call
    match &object.open {
        None if targets.is_empty() => keep(format!(
            "no type the program makes an object of {} meets its bounds: the call never runs",
            object.name
        )),
        None if targets.len() > MAX_TARGETS => keep(format!(
            "{} is made of {} types, more than the {MAX_TARGETS} worth comparing",
            object.name,
            targets.len()
        )),
        // One type: every object of the type is of it, whichever copy of its vtable it holds,
        // so no compare.
        None if targets.len() == 1 => {
            Plan::Rewrite { guarded: Vec::new(), last: Some(targets[0].ty.as_ref()), outcome: "direct" }
        }
        // A compare of each, and the `dyn` call past them: a vtable is one object per content
        // in a staged image, but not in a lazy run, nor where two origins' settings key one
        // method twice (docs/v2/plan/lanes/codegen/devirt.md, "Vtable identity"). Past the
        // compares is where an object with another copy of a listed type's vtable goes.
        None => Plan::Rewrite { guarded: tys(&targets), last: None, outcome: "compared" },
        Some(open) => {
            let hottest = targets
                .iter()
                .copied()
                .filter(|it| it.count.is_some())
                .min_by(|a, b| b.count.cmp(&a.count).then(a.id.0.cmp(&b.id.0)));
            match hottest.filter(|_| speculate) {
                Some(hottest) => {
                    Plan::Rewrite { guarded: tys(&[hottest]), last: None, outcome: "speculated" }
                }
                // alloc: a reason, once a call kept
                None => keep(format!("{} in {} is open: {open}", object.name, world.program)),
            }
        }
    }
}

/// Hottest first where the profile counts every target, else in the world's order (by
/// content); ties by content. Either is the same for the same facts.
fn order(targets: &mut [&Target]) {
    if targets.iter().all(|it| it.count.is_some()) {
        targets.sort_by(|a, b| b.count.cmp(&a.count).then(a.id.0.cmp(&b.id.0)));
    }
}

/// Whether `ty` meets every projection bound of `dyn_ty`: `None` where the solver can't say.
fn bounds_hold<'db>(ctx: &Ctx<'_, 'db>, dyn_ty: Ty<'db>, ty: Ty<'db>) -> Option<bool> {
    let interner = ctx.infcx.interner;
    let TyKind::Dynamic(bounds, _) = dyn_ty.kind() else { return Some(false) };
    for bound in bounds.projection_bounds() {
        let predicate = erased_pred(interner, bound.with_self_ty(interner, ty));
        let obligation = Obligation::new(interner, ObligationCause::dummy(), ctx.env, predicate);
        if ctx.infcx.predicate_must_hold_modulo_regions(&obligation) {
            continue;
        }
        if !ctx.infcx.predicate_may_hold(&obligation) {
            return Some(false);
        }
        return None;
    }
    Some(true)
}

fn erased_pred<'db, T: rustc_type_ir::TypeFoldable<DbInterner<'db>>>(
    interner: DbInterner<'db>,
    value: T,
) -> T {
    rustc_type_ir::fold_regions(interner, value, |_, _| Region::new_erased(interner))
}

/// `block`'s `dyn` call made a compare of the object's vtable against each of `guarded`, a
/// direct call on a match, and past them `last`'s direct call or, without one, the `dyn` call,
/// whose receiver local it gives back.
fn rewrite<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    block: BasicBlockId,
    site: &Site<'db>,
    guarded: &[Ty<'db>],
    last: Option<Ty<'db>>,
) -> Result<'db, Option<LocalId>> {
    let interner = ctx.infcx.interner;
    let Some(Terminator {
        span,
        kind:
            TerminatorKind::Call { func, args, destination, target, cleanup, from_hir_call, intrinsic },
    }) = body.basic_blocks[block].terminator.take()
    else {
        unreachable!("`site` took a call in bb{}", block_index(block));
    };
    let is_cleanup = body.basic_blocks[block].is_cleanup;
    let word = ctx
        .db
        .target_data_layout(ctx.krate())
        .map_err(|_| MirLowerError::LayoutError(crate::layout::LayoutError::TargetLayoutNotAvailable))?
        .pointer_size()
        .bytes_usize();
    let unit_ptr = Ty::new_ptr(interner, Ty::new_unit(interner), Mutability::Not);
    let parts_ty = Ty::new_array(interner, unit_ptr, 2);
    let dyn_ptr = Ty::new_ptr(interner, site.dyn_ty, Mutability::Not);
    let bool_ty = Ty::new_bool(interner);
    let receiver_ty = ctx.normalize(ctx.operand_ty(body, &args[0]));
    let mut local = |ty: Ty<'db>| body.locals.alloc(Local { ty: ty.store() });
    let receiver = local(receiver_ty);
    let parts = local(parts_ty);
    let vtable = local(unit_ptr);
    let data = local(unit_ptr);
    let expected_fat = local(dyn_ptr);
    let expected_parts = local(parts_ty);
    let expected = local(unit_ptr);
    let matched = local(bool_ty);
    let assign = |to: LocalId, rvalue: Rvalue| Statement {
        kind: StatementKind::Assign(Place::from(to).store(), rvalue),
        span,
    };
    let part = |of: LocalId, at: u64| {
        let place = Place::from(of)
            .project(ProjectionElem::ConstantIndex { offset: at, min_length: 2, from_end: false });
        Operand { kind: OperandKind::Copy(place.store()), span: None }
    };
    let copy = |of: LocalId| Operand { kind: OperandKind::Copy(Place::from(of).store()), span: None };
    let mov = |of: LocalId| Operand { kind: OperandKind::Move(Place::from(of).store()), span: None };
    let [receiver_arg, rest @ ..] = &*args else { unreachable!("`site` took a call with a receiver") };
    // The object's two words, by the code generator's layout of a wide pointer: the data
    // pointer, then the metadata.
    body.basic_blocks[block].statements.extend([
        assign(receiver, Rvalue::Use(receiver_arg.clone())), // clone: a refcount
        assign(parts, Rvalue::Cast(CastKind::Transmute, copy(receiver), parts_ty.store())),
        assign(data, Rvalue::Use(part(parts, 0))),
        assign(vtable, Rvalue::Use(part(parts, 1))),
    ]);
    let direct = |body: &mut MirBody<'db>, ty: Ty<'db>| {
        let this = body.locals.alloc(Local { ty: Ty::new_ref(interner, Region::new_erased(interner), ty, site.receiver).store() });
        let this_ty = body.locals[this].ty.as_ref();
        let mut method_args: Vec<GenericArg<'db>> = site.args.iter().collect(); // alloc: a call's generic arguments
        method_args[0] = ty.into();
        let call = TerminatorKind::Call {
            func: Operand::from_fn(ctx.db, site.method, GenericArgs::new_from_slice(&method_args)),
            // alloc: a call's arguments, kept in the body; clone: refcounts
            args: std::iter::once(mov(this)).chain(rest.iter().cloned()).collect(),
            destination: destination.clone(), // clone: a refcount
            target,
            cleanup,
            from_hir_call,
            intrinsic: None,
        };
        body.basic_blocks.alloc(BasicBlock {
            // alloc: the block's statements, kept in the body
            statements: vec![Statement {
                kind: StatementKind::Assign(
                    Place::from(this).store(),
                    Rvalue::Cast(CastKind::Transmute, copy(data), this_ty.store()),
                ),
                span,
            }],
            terminator: Some(Terminator { span, kind: call }),
            is_cleanup,
        })
    };
    let otherwise = match last {
        Some(ty) => direct(body, ty),
        None => {
            // The `dyn` call, past the compares; its receiver read once, above.
            let mut args = args.clone(); // clone: refcounts
            args[0] = copy(receiver);
            body.basic_blocks.alloc(BasicBlock {
                statements: Vec::new(),
                terminator: Some(Terminator {
                    span,
                    kind: TerminatorKind::Call { func, args, destination: destination.clone(), target, cleanup, from_hir_call, intrinsic }, // clone: a refcount
                }),
                is_cleanup,
            })
        }
    };
    // Built back to front: each compare falls through to the next.
    let mut next = otherwise;
    for &ty in guarded.iter().rev() {
        let on_match = direct(body, ty);
        let null = Operand::from_bytes(vec![0; word].into_boxed_slice(), Ty::new_ptr(interner, ty, Mutability::Not)); // alloc: a constant, kept in the body
        let unsize = CastKind::PointerCoercion(PointerCast::Unsize);
        next = body.basic_blocks.alloc(BasicBlock {
            // alloc: the block's statements, kept in the body
            statements: vec![
                assign(expected_fat, Rvalue::Cast(unsize, null, dyn_ptr.store())),
                assign(expected_parts, Rvalue::Cast(CastKind::Transmute, mov(expected_fat), parts_ty.store())),
                assign(expected, Rvalue::Use(part(expected_parts, 1))),
                assign(matched, Rvalue::BinaryOp(BinOp::Eq, copy(vtable), copy(expected))),
            ],
            terminator: Some(Terminator {
                span,
                kind: TerminatorKind::SwitchInt {
                    discr: mov(matched),
                    targets: SwitchTargets::static_if(0, next, on_match),
                },
            }),
            is_cleanup,
        });
    }
    body.basic_blocks[block].terminator =
        Some(Terminator { span, kind: TerminatorKind::Goto { target: next } });
    Ok(last.is_none().then_some(receiver))
}
