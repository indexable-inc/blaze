//! The passes that take a body from [`MirPhase::Built`] to [`MirPhase::Runtime`], as rustc's
//! `run_runtime_lowering_passes` does, in the order they run:
//!
//! 1. [`asserts::add_asserts`]: rustc's `Assert`s before what panics, with their own unwind
//!    edges into the scopes' unwind drop tree, which lowering builds.
//! 2. [`upvars::upvars_in_place`]: an `async` body uses its captures in place.
//! 3. [`cleanup::remove_noop_landing_pads`]: an unwind edge to nothing but storage ends goes.
//! 4. [`elaborate_drops::elaborate_drops`]: every drop drops; drop flags are locals.
//! 5. [`packed_drops::add_moves_for_packed_drops`]: what a packed struct leaves misaligned is
//!    moved out before it is dropped.
//!
//! None of them optimizes. The IDE never asks for these queries, so it never pays for them;
//! the code generator and the interpreter do. `docs/mir-passes.md` has the design.

mod asserts;
mod cleanup;
mod elaborate_drops;
mod move_paths;
mod packed_drops;
mod upvars;

pub use cleanup::noop_landing_pads;
pub(super) use cleanup::remove_noop_landing_pads;

use std::cell::RefCell;

use base_db::{Db, Crate};
use blaze_hashcons::Interned;
use blaze_mem::{Frame, Tag, frame};
use hir_def::{DefWithBodyId, HasModule, attrs::AttrFlags, expr_store::Body};
use rustc_ast_ir::Mutability;
use rustc_type_ir::inherent::{IntoKind, Ty as _};

use crate::{
    InferBodyId, InferenceResult,
    db::{HirDatabase, InternedCoroutineId},
    drop::{DropGlue, has_drop_glue},
    next_solver::{
        DbInterner, ParamEnv, Region, StoredTy, Ty, TyKind,
        TypingMode,
        infer::{DbInternerInferExt, InferCtxt, traits::ObligationCause},
        obligation_ctxt::ObligationCtxt,
    },
    traits::ParamEnvAndCrate,
};

use super::{
    BorrowKind, MirBody, MirLowerError, MirPhase, Operand, OperandKind, Rvalue, StatementKind,
    TerminatorKind,
    budget::Pass,
    framed::{self, FrameMap},
};

type Result<'db, T> = std::result::Result<T, MirLowerError<'db>>;

/// What the passes ask of the types of a body.
pub(super) struct Ctx<'f, 'db> {
    pub(super) db: &'db Db,
    pub(super) infcx: InferCtxt<'db>,
    pub(super) env: ParamEnv<'db>,
    krate: Crate,
    /// The frame of the body's pipeline, which outlives each pass's own.
    pub(super) frame: &'f Frame,
    needs_drop: RefCell<FrameMap<'f, Ty<'db>, bool>>,
}

impl<'f, 'db> Ctx<'f, 'db> {
    pub(super) fn new(db: &'db Db, owner: InferBodyId<'db>, frame: &'f Frame) -> Self {
        let krate = owner.krate(db);
        let env = db.trait_environment(owner.generic_def(db));
        let interner = DbInterner::new_with(db, krate);
        let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
        Ctx { db, infcx, env, krate, frame, needs_drop: RefCell::new(framed::map_in(frame)) }
    }

    /// Whether a value of `ty` may have something to drop: a type parameter may.
    pub(super) fn needs_drop(&self, ty: Ty<'db>) -> bool {
        if let Some(&known) = self.needs_drop.borrow().get(&ty) {
            return known;
        }
        let needs = has_drop_glue(&self.infcx, ty, self.env) != DropGlue::None;
        self.needs_drop.borrow_mut().insert(ty, needs);
        needs
    }

    pub(super) fn normalize(&self, ty: Ty<'db>) -> Ty<'db> {
        if !matches!(ty.kind(), TyKind::Alias(..)) {
            return ty;
        }
        let mut ocx = ObligationCtxt::new(&self.infcx);
        // One the solver can't decide or normalize stays the alias, which `needs_drop` takes as
        // maybe having something to drop: rustc's `Ty::needs_drop`, whose
        // `try_normalize_erasing_regions(..).unwrap_or(query_ty)` keeps it. An error type here
        // would have no drop glue and lose the drop.
        match ocx.structurally_normalize_ty(&ObligationCause::dummy(), self.env, ty) {
            Ok(normalized) if !matches!(normalized.kind(), TyKind::Infer(_)) => normalized,
            _ => ty,
        }
    }

    /// The type of `operand` in `body`, not normalized: a static's is the `&'static T` the
    /// operand is.
    pub(super) fn operand_ty(&self, body: &MirBody<'db>, operand: &Operand) -> Ty<'db> {
        match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => {
                place.as_ref().ty(body, &self.infcx, self.env).ty
            }
            OperandKind::Constant { ty, .. } => ty.as_ref(),
            OperandKind::Allocation { allocation } => allocation.as_ref().ty,
            &OperandKind::Static(static_) => {
                let interner = self.infcx.interner;
                let ty = InferenceResult::of(self.db, DefWithBodyId::from(static_))
                    .expr_ty(Body::of(self.db, static_.into()).root_expr());
                Ty::new_ref(interner, Region::new_static(interner), ty, Mutability::Not)
            }
        }
    }

    fn has_destructor(&self, adt: hir_def::AdtId) -> bool {
        crate::drop::destructor(self.db, adt).is_some()
    }

    /// Whether a value of `ty` has nothing to drop and no cell in it.
    fn is_plain(&self, ty: Ty<'db>) -> bool {
        !self.needs_drop(ty) && crate::drop::is_freeze(&self.infcx, ty, self.env)
    }

    pub(super) fn bool_ty(&self) -> Ty<'db> {
        Ty::new_bool(self.infcx.interner)
    }

    /// The crate the body is of.
    pub(super) fn krate(&self) -> Crate {
        self.krate
    }

    /// The size of `ty` in bytes, if its layout is known where the body is generic.
    pub(super) fn size_of(&self, ty: Ty<'db>) -> Option<u64> {
        Some(self.layout_of(ty)?.size.bytes())
    }

    /// The layout of `ty`, if it is known where the body is generic.
    pub(super) fn layout_of(
        &self,
        ty: Ty<'db>,
    ) -> Option<Interned<'static, crate::layout::InternedLayout>> {
        let env = ParamEnvAndCrate { param_env: self.env, krate: self.krate };
        self.db.layout_of_ty(ty.store(), env.store()).ok()
    }

    /// Whether `ty` is `Sized` where the body is generic: rustc's `Ty::is_sized`.
    pub(super) fn is_sized(&self, ty: Ty<'db>) -> bool {
        self.infcx.type_is_sized_modulo_regions(self.env, ty)
    }

    /// The alignment of `ty`, if its layout is known where the body is generic.
    fn layout_align(&self, ty: Ty<'db>) -> Option<u64> {
        let env = ParamEnvAndCrate { param_env: self.env, krate: self.krate };
        let layout = self.db.layout_of_ty(ty.store(), env.store()).ok()?;
        Some(layout.align.abi.bytes())
    }
}

/// `built` after the passes.
pub(super) fn run_passes<'db>(db: &'db Db, built: &MirBody<'db>) -> Result<'db, MirBody<'db>> {
    // clone: the copy the query keeps; the passes change it in place
    let mut body = built.clone();
    frame::task(Tag::Mir, |outer| -> Result<'db, ()> {
        let ctx = Ctx::new(db, body.owner, outer);
        // rustc's `CleanupPostBorrowck` before these: a false edge, which only borrow
        // checking follows, is a jump to its real target.
        for (_, block) in body.basic_blocks.iter_mut() {
            if let Some(terminator) = &mut block.terminator
                && let TerminatorKind::FalseEdge { real_target, .. } = terminator.kind
            {
                terminator.kind = TerminatorKind::Goto { target: real_target };
            }
        }
        // `Assert`s first: lowering says where each panicking statement unwinds by its
        // position.
        frame::task(Tag::Mir, |frame| asserts::add_asserts(&ctx, &mut body, frame));
        frame::task(Tag::Mir, |frame| upvars::upvars_in_place(&mut body, frame));
        // Counted first, so the kept list is made once at its length; the second
        // `is_plain` of a local hits the `needs_drop` cache.
        let plain = body
            .promoted_locals
            .iter()
            .filter(|&&(local, _)| ctx.is_plain(body.locals[local].ty.as_ref()))
            .count();
        let mut promoted_constants = Vec::with_capacity(plain);
        promoted_constants.extend(
            body.promoted_locals
                .iter()
                .map(|&(local, _)| local)
                .filter(|&local| ctx.is_plain(body.locals[local].ty.as_ref())),
        );
        body.promoted_constants = promoted_constants;
        frame::task(Tag::Mir, |frame| cleanup::remove_noop_landing_pads(&mut body, frame));
        frame::task(Tag::Mir, |frame| elaborate_drops::elaborate_drops(&ctx, &mut body, frame))?;
        frame::task(Tag::Mir, |frame| {
            packed_drops::add_moves_for_packed_drops(&ctx, &mut body, frame)
        });
        Ok(())
    })?;
    body.phase = MirPhase::Runtime;
    body.shrink_to_fit();
    Ok(body)
}

/// One of these passes alone, or drop elaboration's analyses without its rewrite, in `frame`.
pub(super) fn run_one<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    pass: Pass,
    frame: &Frame,
) -> Result<'db, ()> {
    match pass {
        Pass::AddAsserts => asserts::add_asserts(ctx, body, frame),
        Pass::RemoveNoopLandingPads => cleanup::remove_noop_landing_pads(body, frame),
        Pass::ElaborateDrops => elaborate_drops::elaborate_drops(ctx, body, frame)?,
        Pass::DropAnalysis => elaborate_drops::analyse(ctx, body, frame),
        Pass::CleanupPostBorrowck
        | Pass::SimplifyCfg
        | Pass::RemoveStorageMarkers
        | Pass::RemoveUnneededDrops
        | Pass::UnreachablePropagation
        | Pass::InstSimplify
        | Pass::LowerSliceLenCalls
        | Pass::SimplifyConstCondition
        | Pass::SimplifyLocals
        | Pass::SingleUseConsts
        | Pass::DeadStoreElimination
        | Pass::CopyProp
        | Pass::CriticalCallEdges
        | Pass::CheckAlignment
        | Pass::CheckNull
        | Pass::CheckEnums => {
            // alloc: error path
            return Err(MirLowerError::NotSupported(format!(
                "{pass:?} is an optimization, which `optimize::run_one` runs"
            )));
        }
    }
    Ok(())
}

/// The constant behind a promoted `&expr` of `def` after the passes: what the code generator
/// evaluates for it with the interpreter.
#[blaze_query::tracked(returns(as_ref), lru = 0)]
pub fn runtime_promoted_mir_body_query<'db>(
    db: &'db Db,
    def: InferBodyId<'db>,
    expr: hir_def::hir::ExprId,
) -> Result<'db, MirBody<'db>> {
    let built = super::promoted_mir_body_query(db, def, expr).map_err(Clone::clone)?;
    run_passes(db, built)
}

/// The types an `async` block holds across a yield: rustc's `mir_coroutine_witnesses`, the
/// `CoroutineLayout::field_tys` its future implements auto traits by, in the order the body
/// declares the locals. Not what `ignore_for_traits` marks: the temporary made for a place of a
/// static that is not `#[thread_local]` (another access makes it again) and a fake borrow (only
/// fake reads read it). Empty for a body MIR lowering refuses, as rustc's for a body with
/// errors: that body is reported (`BorrowckSkipped`) and nothing is built from it.
pub(crate) fn compute_coroutine_witnesses<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
) -> Box<[StoredTy]> {
    let Ok(body) = db.runtime_mir_body_for_coroutine(coroutine) else {
        return Box::default();
    };
    frame::task(Tag::Mir, |frame| {
        let held = body.held_locals_in(frame);
        let mut ignored: framed::Bits<'_> = framed::bits_in(body.locals.len(), frame);
        for (_, block) in body.basic_blocks.iter() {
            for statement in &block.statements {
                let StatementKind::Assign(place, value) = &statement.kind else { continue };
                if !place.projection.is_empty() {
                    continue;
                }
                let ignore = match value {
                    Rvalue::Use(Operand { kind: OperandKind::Static(id), .. }) => {
                        !AttrFlags::query(db, (*id).into()).contains(AttrFlags::THREAD_LOCAL)
                    }
                    Rvalue::Ref(BorrowKind::Shallow, _) => true,
                    _ => false,
                };
                if ignore {
                    ignored.insert(framed::local_index(place.local));
                }
            }
        }
        held.locals
            .iter()
            .filter(|&&local| !ignored.contains(framed::local_index(local)))
            // clone: a stored type's refcount, kept in the query's result
            .map(|&local| body.locals[local].ty.clone())
            // alloc: the witnesses the query keeps
            .collect()
    })
}
