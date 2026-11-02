//! Loan liveness as rustc's `-Zpolonius=next` computes it: a loan is live where it can reach a
//! live region in a graph whose nodes are a region at a point. Edges within a point are the
//! outlives constraints that hold there; edges between points follow a region while it is
//! live, forwards for a covariant one, backwards for a contravariant one and both ways for an
//! invariant one. A loan is in scope until the first point it is not live at.

use hir_ty::db::HirDatabase;
use blaze_ids::radix;
use rustc_type_ir::{Variance, inherent::IntoKind};

use hir_ty::{
    mir::{MirBody, OperandKind, Rvalue, StatementKind},
    next_solver::{GenericArgKind, GenericArgs, Region, Ty, TyKind},
};

use super::{
    Ctx,
    bitset::bits,
    cfg::{Cfg, Location, index},
    intervals::IntervalSet,
    loans::Loans,
    mem::{FVec, filled},
    regions::{Regions, Rv, rv_of},
    relation::Csr,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Direction {
    Forward,
    Backward,
    Both,
}

/// The points each loan is live at.
pub(super) fn live_loans<'a, 'db>(
    ctx: &Ctx<'a, 'db>,
    body: &MirBody<'db>,
    cfg: &Cfg<'_>,
    regions: &Regions<'_, 'db>,
    count: usize,
    liveness: &[IntervalSet<'_>],
    loans: &Loans<'_>,
) -> FVec<'a, IntervalSet<'a>> {
    let frame = ctx.frame;
    let mut directions = filled(frame, count, None::<Direction>);
    for &ty in &regions.local_tys {
        record_variances(ctx, ty, Variance::Covariant, &mut directions);
    }
    // rust-analyzer reaches a `static mut` through a shared reference, rustc through a
    // `*mut T`, in which `T` is invariant.
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            if let StatementKind::Assign(place, Rvalue::Use(operand)) = &statement.kind
                && let OperandKind::Static(id) = operand.kind
                && place.projection.is_empty()
                && hir_def::signatures::StaticSignature::of(ctx.db, id)
                    .flags
                    .contains(hir_def::signatures::StaticFlags::MUTABLE)
                && let TyKind::Ref(_, inner, _) =
                    regions.local_tys[place.local.into_raw().into_u32() as usize].kind()
            {
                record_variances(ctx, inner, Variance::Invariant, &mut directions);
            }
        }
    }
    // Indexed by region, read on every step of every loan's walk: a universal region flows
    // forwards and is live everywhere; rustc falls back to both ways for a region it has no
    // variance of.
    let mut universal = bits(frame, count);
    for &(rv, _) in &regions.universals {
        if rv < count {
            universal.insert(rv);
        }
    }
    let direction = |rv: Rv| match universal.contains(rv) {
        true => Direction::Forward,
        false => directions.get(rv).copied().flatten().unwrap_or(Direction::Both),
    };
    let all = {
        let mut all = IntervalSet::new(frame);
        if cfg.points > 0 {
            all.insert_range(0, cfg.points - 1);
        }
        all
    };
    let empty = IntervalSet::new(frame);
    let live_at = |rv: Rv| match universal.contains(rv) {
        true => &all,
        false => liveness.get(rv).unwrap_or(&empty),
    };
    // The edges out of each region: those that hold at one point, sorted by point, and those
    // that hold everywhere, each sub once.
    let constraints = &regions.outlives;
    let mut localized: Csr<'_, (u32, u32)> = Csr::with_capacity(frame, count, constraints.len());
    let mut logical: Csr<'_> = Csr::with_capacity(frame, count, constraints.len());
    for sup in 0..count {
        for &at in constraints.of_sup(sup) {
            let sub = constraints.sub(at as usize);
            if sub >= count {
                continue;
            }
            match constraints.location(at as usize) {
                Some(location) => localized.push((cfg.point(location) as u32, sub as u32)),
                None => {
                    if !logical.open().contains(&(sub as u32)) {
                        logical.push(sub as u32);
                    }
                }
            }
        }
        let edges = localized.open_mut();
        radix::sort_copy_by_key(edges, |&(point, sub)| radix::pack_32(point, sub));
        let mut len = 0;
        for at in 0..edges.len() {
            if len == 0 || edges[at] != edges[len - 1] {
                edges[len] = edges[at];
                len += 1;
            }
        }
        localized.truncate_open(len);
        localized.end_node();
        logical.end_node();
    }
    let last_of =
        |block| cfg.point(Location { block, statement: body.basic_blocks[block].statements.len() });

    // The graph's nodes are a region at a point, but it is walked a run of points of one block
    // at a time: what reaches a region at a run of points reaches its subregions at all of
    // them, and a region live at a run of points flows along it.
    let mut live = Vec::with_capacity_in(loans.loans.len(), frame);
    let mut visited = filled(frame, count, IntervalSet::new(frame));
    let mut touched: FVec<'_, Rv> = Vec::new_in(frame);
    let mut stack: FVec<'_, (Rv, usize, usize)> = Vec::new_in(frame);
    let mut runs: FVec<'_, (usize, usize)> = Vec::new_in(frame);
    for loan in loans.loans.iter() {
        let mut points = IntervalSet::new(frame);
        for rv in touched.drain(..) {
            visited[rv].clear();
        }
        let issued = cfg.point(loan.location);
        stack.push((loan.region, issued, issued));
        while let Some((region, start, end)) = stack.pop() {
            if region >= count {
                continue;
            }
            if visited[region].is_empty() {
                touched.push(region);
            }
            let block = cfg.location(start).block;
            let (first, last) = (cfg.point(Location { block, statement: 0 }), last_of(block));
            let region_live = live_at(region);
            let direction = direction(region);
            visited[region].insert_new(start, end, &mut runs);
            for &(start, end) in runs.iter() {
                for &sub in logical.of(region) {
                    stack.push((sub as Rv, start, end));
                }
                let edges = localized.of(region);
                let from = edges.partition_point(|&(point, _)| (point as usize) < start);
                for &(point, sub) in
                    edges[from..].iter().take_while(|&&(point, _)| point as usize <= end)
                {
                    stack.push((sub as Rv, point as usize, point as usize));
                }
                for (s, e) in region_live.runs_in(start, end) {
                    points.insert_range(s, e);
                    if matches!(direction, Direction::Backward | Direction::Both) {
                        if s > first {
                            stack.push((region, s - 1, e - 1));
                        } else {
                            if e > s {
                                stack.push((region, s, e - 1));
                            }
                            let location = Location { block, statement: 0 };
                            for previous in cfg.predecessor_locations(body, location) {
                                let point = cfg.point(previous);
                                stack.push((region, point, point));
                            }
                        }
                    }
                }
                if matches!(direction, Direction::Forward | Direction::Both) {
                    // Past the end of the run, the region flows on while it is live.
                    let mut follow = |next: usize, last: usize| {
                        if region_live.contains(next) {
                            let to =
                                region_live.first_absent_in(next, last).map_or(last, |it| it - 1);
                            stack.push((region, next, to));
                        }
                    };
                    if end < last {
                        follow(end + 1, last);
                    } else {
                        for &successor in cfg.successors(block) {
                            follow(
                                cfg.point(Location { block: successor, statement: 0 }),
                                last_of(successor),
                            );
                        }
                    }
                }
            }
        }
        live.push(points);
    }
    live
}

/// Records the variance each region of `ty` has in it, as rustc's `VarianceExtractor`: a
/// region in two places of different variances is invariant.
fn record_variances<'db>(
    ctx: &Ctx<'_, 'db>,
    ty: Ty<'db>,
    variance: Variance,
    directions: &mut FVec<'_, Option<Direction>>,
) {
    if variance == Variance::Bivariant {
        return;
    }
    match ty.kind() {
        TyKind::Ref(region, inner, mutability) => {
            record_region(region, variance, directions);
            let inner_variance = match mutability {
                rustc_ast_ir::Mutability::Not => variance,
                rustc_ast_ir::Mutability::Mut => variance.xform(Variance::Invariant),
            };
            record_variances(ctx, inner, inner_variance, directions);
        }
        TyKind::RawPtr(inner, mutability) => {
            let inner_variance = match mutability {
                rustc_ast_ir::Mutability::Not => variance,
                rustc_ast_ir::Mutability::Mut => variance.xform(Variance::Invariant),
            };
            record_variances(ctx, inner, inner_variance, directions);
        }
        TyKind::Adt(def, args) => {
            let variances = ctx.db.variances_of(hir_def::GenericDefId::AdtId(def.def_id()));
            record_args(ctx, args, Some(variances.as_slice()), variance, directions);
        }
        TyKind::Tuple(tys) => {
            for ty in tys.iter() {
                record_variances(ctx, ty, variance, directions);
            }
        }
        TyKind::Array(inner, _) | TyKind::Slice(inner) => {
            record_variances(ctx, inner, variance, directions)
        }
        TyKind::FnPtr(sig, _) => {
            let tys = sig.skip_binder().inputs_and_output;
            let last = tys.len().saturating_sub(1);
            for (index, ty) in tys.iter().enumerate() {
                let inner =
                    if index == last { variance } else { variance.xform(Variance::Contravariant) };
                record_variances(ctx, ty, inner, directions);
            }
        }
        TyKind::Dynamic(predicates, region) => {
            record_region(region, variance, directions);
            for predicate in predicates.iter() {
                if let rustc_type_ir::ExistentialPredicate::Trait(it) = predicate.skip_binder() {
                    record_args(
                        ctx,
                        it.args,
                        None,
                        variance.xform(Variance::Invariant),
                        directions,
                    );
                }
                if let rustc_type_ir::ExistentialPredicate::Projection(it) = predicate.skip_binder()
                {
                    record_args(
                        ctx,
                        it.args,
                        None,
                        variance.xform(Variance::Invariant),
                        directions,
                    );
                }
            }
        }
        TyKind::Alias(_, alias) => {
            let variances = match alias.kind {
                rustc_type_ir::AliasTyKind::Opaque { def_id } => {
                    super::regions::opaque_captures(ctx.frame, ctx.db, def_id, alias.args.len())
                        .map(|it| {
                            crate::mem::collect(
                                ctx.frame,
                                it.into_iter().map(|captured| match captured {
                                    true => Variance::Invariant,
                                    false => Variance::Bivariant,
                                }),
                            )
                        })
                }
                _ => None,
            };
            record_args(
                ctx,
                alias.args,
                variances.as_deref(),
                variance.xform(Variance::Invariant),
                directions,
            )
        }
        TyKind::Closure(_, args)
        | TyKind::CoroutineClosure(_, args)
        | TyKind::Coroutine(_, args)
        | TyKind::CoroutineWitness(_, args) => {
            record_args(ctx, args, None, variance.xform(Variance::Invariant), directions)
        }
        TyKind::FnDef(_, args) => record_args(
            ctx,
            args.no_bound_vars().unwrap(),
            None,
            variance.xform(Variance::Invariant),
            directions,
        ),
        _ => (),
    }
}

fn record_args<'db>(
    ctx: &Ctx<'_, 'db>,
    args: GenericArgs<'db>,
    variances: Option<&[Variance]>,
    variance: Variance,
    directions: &mut FVec<'_, Option<Direction>>,
) {
    for (index, arg) in args.iter().enumerate() {
        let param = variances.and_then(|it| it.get(index)).copied().unwrap_or(Variance::Invariant);
        let variance = variance.xform(param);
        match arg.kind() {
            GenericArgKind::Type(ty) => record_variances(ctx, ty, variance, directions),
            GenericArgKind::Lifetime(region) => record_region(region, variance, directions),
            GenericArgKind::Const(_) => (),
        }
    }
}

fn record_region(
    region: Region<'_>,
    variance: Variance,
    directions: &mut FVec<'_, Option<Direction>>,
) {
    let Some(rv) = rv_of(region) else { return };
    let direction = match variance {
        Variance::Covariant => Direction::Forward,
        Variance::Contravariant => Direction::Backward,
        Variance::Invariant => Direction::Both,
        Variance::Bivariant => return,
    };
    if rv >= directions.len() {
        directions.resize(rv + 1, None);
    }
    let entry = &mut directions[rv];
    *entry = match *entry {
        Some(existing) if existing != direction => Some(Direction::Both),
        _ => Some(direction),
    };
}
