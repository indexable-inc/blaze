//! The drop check's errors, where rustc's borrow checker reports them: each local whose drop
//! check it computes (`dropck_local` in `compiler/rustc_borrowck/src/type_check/liveness/
//! trace.rs` at the pinned nightly, `16d1ffac1`) gets E0320 when hir-ty's
//! [`hir_ty::dropck::dropck_outlives`] of its type went past the crate's recursion limit. A
//! type the drop check could not normalize is an error too, as rustc's fulfillment error is.
//!
//! Which locals, as rustc's `liveness::generate` and `trace` pick them:
//!
//! - A "boring" local, whose free regions all outlive a free region of the body by the
//!   constraints its type check found (`compute_relevant_live_locals`), is always checked:
//!   rustc checks it only to report overflows.
//!   - The exception is a polonius run (`-Zpolonius=next`, blaze's `polonius` and
//!     `polonius-flow`) of a body with a borrow. There a boring local whose regions are not
//!     all universal, and which has no drop at all, is deferred unchecked
//!     (`dropck_boring_local`).
//! - Every other ("relevant") local is checked when some drop of it may find it initialized
//!   (`compute_drop_live_points_for`'s `drop_live_at` is not empty).
//!
//! Each local reports at its own span: rustc's cache is by type, but each `dropck_local` call
//! reports. The drop check of each type runs once per body, in a probe, so it leaves the
//! body's inference as it was.

use hir_def::DefWithBodyId;
use hir_ty::{
    InferBodyId,
    dropck::{DropckError, dropck_outlives},
    next_solver::{Ty, fulfill::NextSolverError},
};
use hir_ty_mir::mir::{LocalId, MirBody, MirSpan};

use super::{
    BorrowckDiagnostic, Ctx, DropCheckOverflow, Mode,
    bitset::bits,
    mem::{self, FVec},
    regions::{Regions, Rv, regions_of},
};

/// Per local, what liveness found of its drops: [`ANY_DROP`] and [`INITIALIZED_DROP`].
pub(super) type DropFlags<'f> = FVec<'f, u8>;

/// Some `Drop` of the local is in the body.
pub(super) const ANY_DROP: u8 = 1;
/// Some `Drop` of the local may find it initialized: rustc's `drop_live_at` is not empty.
pub(super) const INITIALIZED_DROP: u8 = 2;

/// The drop check's errors for the locals of `body`, as the module docs pick them, pushed to
/// `out` in the order of the locals. `has_borrows` is whether the body makes a loan, rustc's
/// `borrow_set.len() != 0`.
pub(super) fn report<'o, 'db>(
    ctx: &Ctx<'_, 'db>,
    body: &MirBody<'db>,
    regions: &Regions<'_, 'db>,
    drops: &DropFlags<'_>,
    count: usize,
    has_borrows: bool,
    out: &mut FVec<'o, BorrowckDiagnostic>,
) {
    let frame = ctx.frame;
    let free = outlive_free_regions(ctx, regions, count);
    let deferring = matches!(ctx.mode, Mode::Polonius | Mode::PoloniusFlow) && has_borrows;
    // rustc's `drop_data` cache: each type's drop check once per body.
    let mut checked: mem::FMap<'_, Ty<'db>, Outcome<'db>> = mem::map(frame);
    let mut scratch: FVec<'_, Rv> = Vec::new_in(frame);
    for (local, &ty) in regions.local_tys.iter().enumerate() {
        let flags = drops.get(local).copied().unwrap_or(0);
        scratch.clear();
        regions_of(ty, &mut scratch);
        let boring = scratch.iter().all(|&rv| free.contains(rv));
        let checks = match boring {
            true => {
                let deferred =
                    deferring && !scratch.iter().all(|&rv| regions.universal(rv).is_some());
                !(deferred && flags & ANY_DROP == 0)
            }
            false => flags & INITIALIZED_DROP != 0,
        };
        if !checks {
            continue;
        }
        let outcome = *checked.entry(ty).or_insert_with(|| check_ty(ctx, ty));
        let local = LocalId::from_raw(la_arena::RawIdx::from_u32(local as u32));
        match outcome {
            Outcome::Fine => {}
            Outcome::Overflow { overflow_ty, limit } => {
                out.push(overflow(ctx, body, local, ty, overflow_ty, limit));
            }
            Outcome::Unnormalized { failed, overflowed } => {
                out.push(unnormalized(ctx, body, local, ty, failed, overflowed));
            }
        }
    }
}

/// What the drop check of one type found.
#[derive(Clone, Copy)]
enum Outcome<'db> {
    Fine,
    /// Its walk went past `limit` at `overflow_ty`, the first type it overflowed on.
    Overflow {
        overflow_ty: Ty<'db>,
        limit: u32,
    },
    /// It could not normalize `failed`; `overflowed` when the solver ran out of depth.
    Unnormalized {
        failed: Ty<'db>,
        overflowed: bool,
    },
}

fn check_ty<'db>(ctx: &Ctx<'_, 'db>, ty: Ty<'db>) -> Outcome<'db> {
    let infcx = &ctx.infcx;
    let limit = rustc_type_ir::Interner::recursion_limit(infcx.interner) as u32;
    infcx.probe(|_| match dropck_outlives(infcx, ctx.env, ty, ctx.frame) {
        Ok(result) => match result.overflows.first() {
            Some(&overflow_ty) => Outcome::Overflow {
                overflow_ty: infcx.deeply_resolve_ignoring_regions(overflow_ty),
                limit,
            },
            None => Outcome::Fine,
        },
        Err(DropckError { ty, errors }) => Outcome::Unnormalized {
            failed: infcx.deeply_resolve_ignoring_regions(ty),
            overflowed: errors.iter().any(|it| matches!(it, NextSolverError::Overflow(_))),
        },
    })
}

/// rustc's `regions_that_outlive_free_regions`: the universal regions and every region the
/// outlives constraints so far make outlive one of them, found over the constraints reversed
/// (`R1: R2` an edge from `R2` to `R1`), as rustc's reverse graph has no edges of its own
/// from `'static`.
fn outlive_free_regions<'f>(
    ctx: &Ctx<'f, '_>,
    regions: &Regions<'_, '_>,
    count: usize,
) -> super::bitset::BitSet<'f> {
    let frame = ctx.frame;
    let constraints = &regions.outlives;
    // The constraints by `sub`, as a CSR: the regions each region is outlived by.
    let mut starts: FVec<'_, u32> = mem::filled(frame, count + 1, 0);
    for at in 0..constraints.len() {
        let sub = constraints.sub(at);
        if sub < count && constraints.sup(at) < count {
            starts[sub + 1] += 1;
        }
    }
    for at in 0..count {
        starts[at + 1] += starts[at];
    }
    let mut fill: FVec<'_, u32> = mem::collect(frame, starts.iter().copied());
    let mut sups: FVec<'_, u32> = mem::filled(frame, starts[count] as usize, 0);
    for at in 0..constraints.len() {
        let (sub, sup) = (constraints.sub(at), constraints.sup(at));
        if sub < count && sup < count {
            sups[fill[sub] as usize] = sup as u32;
            fill[sub] += 1;
        }
    }
    let mut free = bits(frame, count);
    let mut stack: FVec<'_, Rv> = Vec::new_in(frame);
    for &(rv, _) in regions.universals.iter() {
        if rv < count && !free.contains(rv) {
            free.insert(rv);
            stack.push(rv);
        }
    }
    while let Some(sub) = stack.pop() {
        for &sup in &sups[starts[sub] as usize..starts[sub + 1] as usize] {
            let sup = sup as usize;
            if !free.contains(sup) {
                free.insert(sup);
                stack.push(sup);
            }
        }
    }
    free
}

/// rustc's `DropCheckOverflow` (E0320) at `local`'s span, its message naming the body's owner
/// and the limit as well (rule 19).
fn overflow<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &MirBody<'db>,
    local: LocalId,
    ty: Ty<'db>,
    overflow_ty: Ty<'db>,
    limit: u32,
) -> BorrowckDiagnostic {
    let owner = owner_name(ctx);
    let span = hir_ty_mir::mir::local_span(ctx.db, ctx.owner, body, local, MirSpan::Unknown);
    // alloc: an error's owned message
    let message = format!(
        "overflow while adding drop-check rules for a local of `{owner}`: its type nests deeper \
         than the recursion limit of {limit}"
    );
    let mut diagnostic = BorrowckDiagnostic::new("E0320", span, message);
    diagnostic.drop_check = Some(DropCheckOverflow {
        ty: erase(ctx, ty).store(),
        overflow_ty: erase(ctx, overflow_ty).store(),
        limit,
    });
    diagnostic
}

/// rustc's fulfillment error of a type the drop check could not normalize, at `local`'s span:
/// E0275 when the solver ran out of depth.
fn unnormalized<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &MirBody<'db>,
    local: LocalId,
    ty: Ty<'db>,
    failed: Ty<'db>,
    overflowed: bool,
) -> BorrowckDiagnostic {
    let owner = owner_name(ctx);
    let span = hir_ty_mir::mir::local_span(ctx.db, ctx.owner, body, local, MirSpan::Unknown);
    let limit = rustc_type_ir::Interner::recursion_limit(ctx.infcx.interner);
    let (code, why) = match overflowed {
        true => ("E0275", format!("overflow past the recursion limit of {limit}")), // alloc: an error's owned message
        false => ("", String::from("the trait solver cannot prove it")), // alloc: an error's owned message
    };
    // alloc: an error's owned message
    let message = format!(
        "the drop check of a local of `{owner}`, of type `{:?}`, cannot normalize `{:?}`: {why}",
        erase(ctx, ty),
        erase(ctx, failed),
    );
    BorrowckDiagnostic::new(code, span, message)
}

/// `ty` with every region erased, as rustc prints a type in a message.
fn erase<'db>(ctx: &Ctx<'_, 'db>, ty: Ty<'db>) -> Ty<'db> {
    let interner = ctx.infcx.interner;
    rustc_type_ir::fold_regions(interner, ty, |region, _| match region.kind() {
        rustc_type_ir::RegionKind::ReBound(..) => region,
        _ => <hir_ty::next_solver::Region<'db> as hir_ty::next_solver::RegionExt<'db>>::new_erased(
            interner,
        ),
    })
}

/// The name of the item whose body this is, as a message names it.
fn owner_name(ctx: &Ctx<'_, '_>) -> String {
    use hir_def::signatures::{ConstSignature, FunctionSignature, StaticSignature};
    let db = ctx.db;
    match ctx.owner {
        InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(it)) => {
            FunctionSignature::of(db, it).name.as_str().to_owned() // alloc: an error's owned message
        }
        InferBodyId::DefWithBodyId(DefWithBodyId::StaticId(it)) => {
            StaticSignature::of(db, it).name.as_str().to_owned() // alloc: an error's owned message
        }
        InferBodyId::DefWithBodyId(DefWithBodyId::ConstId(it)) => ConstSignature::of(db, it)
            .name
            .as_ref()
            .map_or_else(|| "_".to_owned(), |it| it.as_str().to_owned()), // alloc: an error's owned message
        InferBodyId::DefWithBodyId(DefWithBodyId::VariantId(it)) => it
            .loc(db)
            .parent
            .enum_variants(db)
            .variants
            .iter()
            .find(|(_, (variant, _))| *variant == it)
            .map_or_else(|| "_".to_owned(), |(name, _)| name.as_str().to_owned()), // alloc: an error's owned message
        InferBodyId::AnonConstId(_) => "a constant".to_owned(), // alloc: an error's owned message
    }
}
