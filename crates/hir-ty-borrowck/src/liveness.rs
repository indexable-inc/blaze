//! Liveness, as rustc's `type_check/liveness`: a region holds at each point where a local
//! whose type has it may still be used, and where a local may still be dropped while it may
//! be initialized, for the regions dropping it uses.
//!
//! What each point uses and defines is the decode's ([`Decoded::uses`], [`Decoded::defs`]);
//! the reads a callee's signature makes and the drops of what may be initialized are the
//! facts found here.

use base_db::Db;
use blaze_ids::radix;
use hir_def::{AdtId, attrs::AttrFlags};
use rustc_type_ir::{
    ClauseKind, OutlivesClause, RegionKind,
    inherent::{
        AdtDef as _, GenericArgs as _, IntoKind, ParamEnv as _, SliceLike as _,
    },
};

use hir_ty::{
    db::HirDatabase,
    mir::{BasicBlockId, MirBody},
    next_solver::{
        DbInterner, GenericArgKind, GenericArgs, Region, RegionExt as _, StoredRegion, StoredTy,
        Ty, TyKind,
    },
};

use super::{
    Ctx,
    bitset::{BitMatrix, BitSet, Transfer, bits, matrix, union_words},
    cfg::{Cfg, index},
    dataflow::{backward, forward},
    decode::{Decoded, NONE, raw},
    intervals::IntervalSet,
    mem::{self, FMap, FVec, Frame, filled},
    moves::{InitKind, MoveData},
    regions::{Regions, Rv, regions_of, rv_of},
    relation::{Csr, Facts, Relation},
    rows::{Edge, Effects},
};

/// The kinds of the facts the decode does not have: the locals a callee's signature reads at a
/// point, and the locals a point drops while they may be initialized.
const READ: u8 = 0;
const DROP: u8 = 1;

/// What liveness asks of a point.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Gens {
    /// The locals it uses.
    Uses,
    /// The locals it drops.
    Drops,
}

/// What each point does to the locals.
struct Points<'r> {
    decoded: &'r Decoded<'r>,
    facts: Relation<'r>,
}

impl<'r> Points<'r> {
    /// The locals `point` makes live, in two parts.
    fn gens(&self, gens: Gens, point: usize) -> (&'r [u32], &'r [u32]) {
        match gens {
            Gens::Uses => (self.decoded.uses.of(point), self.facts.get(READ, point)),
            Gens::Drops => (&[], self.facts.get(DROP, point)),
        }
    }

    /// The locals `point` defines: assigns whole, or begins or ends the storage of.
    fn defs(&self, point: usize) -> &'r [u32] {
        self.decoded.defs.of(point)
    }
}

/// A fake read of the value an expression statement leaves, which rustc's MIR does not read:
/// liveness and the explanation of a borrow error both pass over it.
pub(super) fn reads_nothing<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &MirBody<'db>,
    place: &hir_ty::mir::StoredPlace,
    span: hir_ty_mir::mir::MirSpan,
) -> bool {
    let hir_ty_mir::mir::MirSpan::ExprId(expr) = span else { return false };
    let Some(value) = ctx.scan.statement_value(expr) else { return false };
    value.is_none_or(|value| {
        place.projection.is_empty()
            && rustc_type_ir::fold_regions(
                ctx.infcx.interner,
                body.locals[place.local].ty.as_ref(),
                |_, _| Region::new_erased(ctx.infcx.interner),
            ) == value
    })
}

/// The points each local is live at, by a backward analysis where `gens` says which locals a
/// point makes live, as ranges: what liveness gives a region is the points of the locals it is
/// in. `edge` restricts what is live coming back along an edge.
fn live_ranges<'f, 'e>(
    frame: &'f Frame,
    body: &MirBody<'_>,
    cfg: &Cfg<'_>,
    points: &Points<'_>,
    gens: Gens,
    edge: impl Fn(BasicBlockId, BasicBlockId) -> Edge<'e>,
) -> FVec<'f, IntervalSet<'f>> {
    let locals = body.locals.len();
    let mut transfer = Effects::new(frame, body.basic_blocks.len(), locals);
    for &block in cfg.order.iter() {
        let mut row = transfer.row(index(block));
        for point in (cfg.block_start(block)..=cfg.block_end(body, block)).rev() {
            for &local in points.defs(point) {
                row.kill(local as usize);
            }
            let (some, more) = points.gens(gens, point);
            for &local in some.iter().chain(more) {
                row.gen_(local as usize);
            }
        }
    }
    let entry = backward(frame, cfg, &transfer, &edge);
    let mut ranges = filled(frame, locals, IntervalSet::new(frame));
    // Where the run of points each live local is live at ends, walking backwards.
    let run_end = frame.alloc_slice_fill_with(locals, |_| 0usize);
    let mut live = bits(frame, locals);
    let along = frame.alloc_slice_fill_with(live.words().len(), |_| 0u64);
    for &block in cfg.order.iter() {
        live.clear();
        for &successor in cfg.successors(block) {
            entry.load(index(successor), along);
            edge(block, successor).apply(along);
            union_words(live.words_mut(), along);
        }
        let start = cfg.block_start(block);
        let end = cfg.block_end(body, block);
        for local in live.iter() {
            run_end[local] = end;
        }
        for point in (start..=end).rev() {
            let (some, more) = points.gens(gens, point);
            // A local is live at a point that uses it, and where it is assigned if it is live
            // after, as rustc's `compute_use_live_points_for` has it; it is not live before
            // the assignment.
            for &local in some.iter().chain(more) {
                let local = local as usize;
                if live.insert(local) {
                    run_end[local] = point;
                }
            }
            for &local in points.defs(point) {
                if some.contains(&local) || more.contains(&local) {
                    continue;
                }
                let local = local as usize;
                if live.contains(local) {
                    live.remove(local);
                    ranges[local].insert_range(point, run_end[local]);
                }
            }
        }
        for local in live.iter() {
            ranges[local].insert_range(start, run_end[local]);
        }
    }
    ranges
}

/// The value of each region from liveness alone.
pub(super) fn region_liveness<'a, 'db>(
    ctx: &Ctx<'a, 'db>,
    body: &MirBody<'db>,
    cfg: &Cfg<'_>,
    decoded: &Decoded<'a>,
    moves: &MoveData<'_>,
    regions: &Regions<'_, 'db>,
    count: usize,
) -> FVec<'a, IntervalSet<'a>> {
    let frame = ctx.frame;
    let mut values = filled(frame, count, IntervalSet::new(frame));
    // A drop of what cannot be initialized there does nothing.
    let (drops_initialized, init_at_exit) =
        maybe_initialized_at_drops(frame, body, cfg, decoded, moves);
    let mut facts = Facts::with_capacity(frame, regions.callee_reads.len() + cfg.order.len());
    for &(local, location) in regions.callee_reads.iter() {
        facts.push(READ, cfg.point(location), raw(local));
    }
    for &block in cfg.order.iter() {
        let dropped = decoded.dropped[index(block)];
        let point = cfg.block_end(body, block);
        if dropped != NONE && drops_initialized.contains(point) {
            facts.push(DROP, point, dropped as usize);
        }
    }
    let effects = Points { decoded, facts: facts.freeze(&[cfg.points; 2]) };
    let mut local_regions =
        Csr::with_capacity(frame, regions.local_tys.len(), regions.local_tys.len());
    let mut scratch: FVec<'_, Rv> = Vec::new_in(frame);
    for &ty in regions.local_tys.iter() {
        scratch.clear();
        live_regions_of(ctx, regions, ty, &mut scratch);
        for &rv in scratch.iter() {
            local_regions.push(rv);
        }
        local_regions.end_node();
    }
    let use_live = live_ranges(frame, body, cfg, &effects, Gens::Uses, |_, _| Edge::Same);
    for (local, points) in use_live.iter().enumerate() {
        if points.is_empty() {
            continue;
        }
        for &rv in local_regions.of(local) {
            if rv < count {
                values[rv].union(points);
            }
        }
    }
    let init_at_exit = &init_at_exit;
    let mut drop_live = live_ranges(
        frame,
        body,
        cfg,
        &effects,
        Gens::Drops,
        // Dropping a local makes it live back to where it may be initialized only, as
        // rustc's `compute_drop_live_points_for` has it.
        move |block, successor| init_at_exit.edge(block, successor),
    );
    // Where a local is use live, what dropping it uses is live already.
    let mut dropped: FVec<'_, usize> = Vec::new_in(frame);
    for (local, points) in drop_live.iter_mut().enumerate() {
        points.subtract(&use_live[local]);
        if !points.is_empty() {
            dropped.push(local);
        }
    }
    if dropped.is_empty() {
        return values;
    }
    // What dropping each local uses, found in the order a walk of the blocks in reverse
    // postorder, each from its end, first meets the local at a point it is drop live at only:
    // finding it may normalize, which makes region variables.
    let rank = frame.alloc_slice_fill_with(cfg.points, |_| usize::MAX);
    let mut next = 0;
    for &block in cfg.order.iter() {
        for point in (cfg.block_start(block)..=cfg.block_end(body, block)).rev() {
            rank[point] = next;
            next += 1;
        }
    }
    let rank: &[usize] = rank;
    let block_end = |point| cfg.block_end(body, cfg.location(point).block);
    // Each key holds its local, so the keys are distinct and an unstable sort is stable; in the
    // frame, so a body of any size sorts them without the global heap.
    let keys = frame.alloc_slice_fill_with(dropped.len(), |at| {
        let local = dropped[at];
        let first = least_rank(&drop_live[local], rank, cfg.points, block_end);
        radix::pack_64(first as u64, local as u64)
    });
    keys.sort_unstable();
    for (local, &key) in dropped.iter_mut().zip(keys.iter()) {
        *local = key as u64 as usize;
    }
    let mut components = DropComponents::new(ctx, regions);
    for &local in dropped.iter() {
        for &rv in components.of(regions.local_tys[local]) {
            if rv < count {
                values[rv].union(&drop_live[local]);
            }
        }
    }
    values
}

/// The least of `rank` over `points`, of `count` points in all, where `block_end` is the last
/// point of a point's block. Ranks fall as points rise within a block (each block is numbered
/// from its end), so a run's least is at the last point of each block it covers: one look per
/// block a run covers, not per point.
fn least_rank(
    points: &IntervalSet<'_>,
    rank: &[usize],
    count: usize,
    block_end: impl Fn(usize) -> usize,
) -> usize {
    let mut first = usize::MAX;
    for (start, end) in points.runs_in(0, count.saturating_sub(1)) {
        let mut at = start;
        while at <= end {
            let last = block_end(at).min(end);
            first = first.min(rank[last]);
            at = last + 1;
        }
    }
    first
}

/// Which drops drop what may be initialized there, by point; and for each edge between
/// blocks, the locals that may be initialized, if partly, leaving the first for the second.
/// rustc's `MaybeInitializedPlaces`.
pub(super) fn maybe_initialized_at_drops<'f>(
    frame: &'f Frame,
    body: &MirBody<'_>,
    cfg: &Cfg<'_>,
    decoded: &Decoded<'f>,
    moves: &MoveData<'_>,
) -> (BitSet<'f>, InitAtExit<'f>) {
    let paths = moves.len();
    let blocks = body.basic_blocks.len();
    let locals = body.locals.len();
    let mut transfer = Effects::new(frame, blocks, paths);
    let returns = decoded.returns;
    // A call's return: a row per reachable call rather than per block.
    let calls = cfg.order.iter().filter(|&&block| returns[index(block)] != NONE).count();
    let mut on_return = Effects::new(frame, calls, paths);
    // Per block, its call's row.
    let call_of: &'f mut [u32] = frame.alloc_slice_fill_with(blocks, |_| NONE);
    let mut call = 0;
    for &block in cfg.order.iter() {
        let end = cfg.block_end(body, block);
        {
            let mut row = transfer.row(index(block));
            for point in cfg.block_start(block)..=end {
                maybe_init_effect(moves, point, &mut row, false);
            }
        }
        if returns[index(block)] != NONE {
            call_of[index(block)] = call as u32;
            maybe_init_effect(moves, end, &mut on_return.row(call), true);
            call += 1;
        }
    }
    let call_of: &[u32] = call_of;
    let mut start = bits(frame, paths);
    for init in moves.inits.iter().filter(|it| it.location.is_none()) {
        start.gen_range(moves.subtree(init.path));
    }
    let on_return = &on_return;
    let entry = forward(frame, cfg, &transfer, body.start_block, start.words(), move |from, to| {
        let at = index(from);
        if call_of[at] != NONE && returns[at] == index(to) as u32 {
            on_return.edge(call_of[at] as usize)
        } else {
            Edge::Same
        }
    });
    let mut exit = matrix(frame, blocks, locals);
    let mut returned = matrix(frame, blocks, locals);
    let mut reached = bits(frame, blocks);
    let mut found = bits(frame, cfg.points);
    let mut state = bits(frame, paths);
    for &block in cfg.order.iter() {
        let at = index(block);
        reached.insert(at);
        entry.load(at, state.words_mut());
        let end = cfg.block_end(body, block);
        for point in cfg.block_start(block)..end {
            maybe_init_effect(moves, point, &mut state, false);
        }
        let dropped = decoded.dropped[at];
        if dropped != NONE
            && moves.first_in(moves.local_path(super::decode::local(dropped)), &state).is_some()
        {
            found.insert(end);
        }
        maybe_init_effect(moves, end, &mut state, false);
        locals_in(moves, &state, |local| {
            exit.insert(at, local);
        });
        if returns[at] != NONE {
            maybe_init_effect(moves, end, &mut state, true);
            locals_in(moves, &state, |local| {
                returned.insert(at, local);
            });
        }
    }
    (found, InitAtExit { exit, returned, returns, reached })
}

/// What the statement or terminator at `point` does to what may be initialized; with
/// `returned`, what the call there returning does on its return edge instead.
fn maybe_init_effect(
    moves: &MoveData<'_>,
    point: usize,
    state: &mut impl Transfer,
    returned: bool,
) {
    if !returned {
        for &it in moves.moves_at(point) {
            state.kill_range(moves.subtree(moves.moves[it as usize].path));
        }
    }
    for &it in moves.inits_at(point) {
        let init = moves.inits[it as usize];
        if (init.kind == InitKind::Deep) != returned {
            state.gen_range(moves.subtree(init.path));
        }
    }
}

/// Each local one of whose paths is in `state`: the local may be initialized, if partly. A
/// local's paths are one range, so this is a scan of the words that skips each local found.
fn locals_in(moves: &MoveData<'_>, state: &BitSet<'_>, mut found: impl FnMut(usize)) {
    let mut at = 0;
    while let Some(path) = state.first_in(at..moves.len()) {
        let local = moves.local(path);
        found(raw(local));
        at = moves.subtree(moves.local_path(local)).end;
    }
}

/// The locals that may be initialized, if partly, leaving each block, and leaving a call's
/// block for the call's return.
pub(super) struct InitAtExit<'f> {
    pub(super) exit: BitMatrix<'f>,
    pub(super) returned: BitMatrix<'f>,
    /// Per block, the block its call returns to: the decode's.
    returns: &'f [u32],
    /// The blocks the analysis reached, which nothing leaves otherwise.
    pub(super) reached: BitSet<'f>,
}

impl InitAtExit<'_> {
    /// What of `live` crosses from `successor` back to the end of `block`: the locals that
    /// may be initialized, if partly, leaving `block` for `successor`.
    fn edge(&self, block: BasicBlockId, successor: BasicBlockId) -> Edge<'_> {
        let at = index(block);
        if !self.reached.contains(at) {
            return Edge::Nothing;
        }
        if self.returns[at] == index(successor) as u32 {
            Edge::Mask(self.returned.row(at).into_words())
        } else {
            Edge::Mask(self.exit.row(at).into_words())
        }
    }
}

/// The regions dropping a value of a type may use: rustc's dropck outlives constraints, found
/// over the graph of the types dropping it drops. A recursive type and the types it holds
/// that hold it again are one strongly connected component of that graph with one answer, so
/// each type is expanded once, as rustc's dropck keeps a set of the types it has seen; walking
/// the tree of drops instead is exponential in a type like `Value(Vec<Value>, Box<Value>)`.
///
/// Every answer is a range of one pool, shared by the members of a component. What a type
/// uses and the types it drops are kept on two stacks shared by the whole walk: an open
/// component's regions stay where its members left them until its root takes them all.
struct DropComponents<'c, 'a, 'r, 'db> {
    ctx: &'c Ctx<'a, 'db>,
    regions: &'c Regions<'r, 'db>,
    done: FMap<'a, Ty<'db>, (u32, u32)>,
    pool: FVec<'a, Rv>,
    /// Tarjan's: each type on the stack and its number in the walk.
    open: FMap<'a, Ty<'db>, usize>,
    stack: FVec<'a, Ty<'db>>,
    found: FVec<'a, Rv>,
    children: FVec<'a, Ty<'db>>,
    next: usize,
}

/// How deep in the walk a type stops being expanded, which bounds types that grow as they
/// recurse, `struct S<T>(Option<Box<S<(T, T)>>>)`.
const DROP_DEPTH: usize = 12;

impl<'c, 'a, 'r, 'db> DropComponents<'c, 'a, 'r, 'db> {
    fn new(ctx: &'c Ctx<'a, 'db>, regions: &'c Regions<'r, 'db>) -> Self {
        let frame = ctx.frame;
        DropComponents {
            ctx,
            regions,
            done: mem::map(frame),
            pool: Vec::new_in(frame),
            open: mem::map(frame),
            stack: Vec::new_in(frame),
            found: Vec::new_in(frame),
            children: Vec::new_in(frame),
            next: 0,
        }
    }

    fn of(&mut self, ty: Ty<'db>) -> &[Rv] {
        self.visit(ty, 0);
        self.done.get(&ty).map_or(&[], |&(from, to)| &self.pool[from as usize..to as usize])
    }

    /// Walks `ty`: `Some(low)`, the lowest number it reaches, while its component is open.
    fn visit(&mut self, ty: Ty<'db>, depth: usize) -> Option<usize> {
        if depth > DROP_DEPTH || self.done.contains_key(&ty) {
            return None;
        }
        if let Some(&number) = self.open.get(&ty) {
            return Some(number);
        }
        let number = self.next;
        self.next += 1;
        let at = self.stack.len();
        self.open.insert(ty, number);
        self.stack.push(ty);
        let found = self.found.len();
        let children = self.children.len();
        expand(self.ctx, self.regions, ty, &mut self.found, &mut self.children);
        let mine = self.children.len();
        let mut low = number;
        for child in children..mine {
            let child = self.children[child];
            match self.visit(child, depth + 1) {
                Some(it) => low = low.min(it),
                None => {
                    if let Some(&(from, to)) = self.done.get(&child) {
                        self.found.extend_from_slice(&self.pool[from as usize..to as usize]);
                    }
                }
            }
        }
        self.children.truncate(children);
        if low < number {
            return Some(low);
        }
        let all = &mut self.found[found..];
        radix::sort(all);
        let from = self.pool.len() as u32;
        for (index, &rv) in all.iter().enumerate() {
            if index == 0 || all[index - 1] != rv {
                self.pool.push(rv);
            }
        }
        let range = (from, self.pool.len() as u32);
        self.found.truncate(found);
        for member in self.stack.drain(at..) {
            self.open.remove(&member);
            self.done.insert(member, range);
        }
        None
    }
}

/// The regions dropping a `ty` uses itself, and the types it drops.
fn expand<'db>(
    ctx: &Ctx<'_, 'db>,
    regions: &Regions<'_, 'db>,
    ty: Ty<'db>,
    found: &mut FVec<'_, Rv>,
    children: &mut FVec<'_, Ty<'db>>,
) {
    match ty.kind() {
        // Dropping a `ManuallyDrop` drops nothing.
        TyKind::Adt(def, _) if def.is_manually_drop() => (),
        TyKind::Adt(def, args) => {
            let adt = def.def_id();
            if def.is_box() || def.is_phantom_data() {
                if let Some(GenericArgKind::Type(inner)) =
                    args.iter().next().map(|it| it.kind())
                {
                    children.push(inner);
                }
                return;
            }
            let interner = ctx.infcx.interner;
            let constraint = drop_constraint(ctx.db, adt);
            for region in &constraint.regions {
                let region = rustc_type_ir::EarlyBinder::bind(interner, region.as_ref())
                    .instantiate(interner, args)
                    .skip_norm_wip();
                found.extend(super::regions::rv_of(region));
            }
            for whole in &constraint.whole {
                let whole = rustc_type_ir::EarlyBinder::bind(interner, whole.as_ref())
                    .instantiate(interner, args)
                    .skip_norm_wip();
                regions_of(whole, found);
            }
            children.extend(constraint.tys.iter().map(|it| {
                rustc_type_ir::EarlyBinder::bind(interner, it.as_ref())
                    .instantiate(interner, args)
                    .skip_norm_wip()
            }));
        }
        TyKind::Array(inner, _) | TyKind::Slice(inner) => children.push(inner),
        TyKind::Tuple(tys) => children.extend(tys.iter()),
        TyKind::Closure(_, args) => children.push(args.as_closure().tupled_upvars_ty()),
        // rustc's dropck drops what a coroutine captures too, all of it where what it keeps
        // across an `await` needs dropping, which is not known here.
        TyKind::CoroutineClosure(_, args) => {
            children.push(args.as_coroutine_closure().tupled_upvars_ty())
        }
        TyKind::Coroutine(_, args) => children.push(args.as_coroutine().tupled_upvars_ty()),
        TyKind::Dynamic(..) => regions_of(ty, found),
        // A projection is what it normalizes to, `NoisyDrop<T>` of `<T as Associator>::As`;
        // another alias holds what its bounds and captures let it, as for liveness.
        TyKind::Alias(_, alias) => {
            let normalized = match alias.kind {
                rustc_type_ir::AliasTyKind::Projection { .. } => {
                    super::regions::normalize(ctx, ty)
                        .filter(|it| !matches!(it.kind(), TyKind::Alias(_, _) | TyKind::Infer(_)))
                }
                _ => None,
            };
            match normalized {
                Some(it) => children.push(it),
                None => live_regions_of(ctx, regions, ty, found),
            }
        }
        _ => (),
    }
}

/// What dropping a value of an ADT other than `Box`, `PhantomData` and `ManuallyDrop` uses, in
/// its own generic parameters: rustc's `adt_dtorck_constraint`. Its fields are taken apart as
/// far as their types tell without an instantiation, an ADT in them by its own constraint;
/// what is left, parameters, aliases and the types an ADT in them leaves, is taken apart where
/// the ADT is dropped, which is also what keeps two ADTs that hold each other from asking each
/// other. Once per ADT, so a body that drops a `Value` instantiates what that leaves rather
/// than every field of every variant.
#[derive(Debug, Clone, PartialEq, Eq, Default, blaze_query::Value)]
struct DropConstraint {
    /// The lifetimes its `Drop` impl uses.
    regions: Vec<StoredRegion>,
    /// Types all of whose regions dropping it uses: those its `Drop` impl uses, trait objects.
    whole: Vec<StoredTy>,
    /// The types dropping it drops, taken apart where it is dropped.
    tys: Vec<StoredTy>,
}

#[blaze_query::tracked(returns(ref), cycle_result = drop_constraint_cycle)]
fn drop_constraint(db: &Db, adt: AdtId) -> DropConstraint {
    blaze_mem::frame::task(blaze_mem::Tag::Mir, |frame| {
        let interner = DbInterner::new_no_crate(db);
        let mut regions: FVec<'_, Region<'_>> = Vec::new_in(frame);
        let mut whole: FVec<'_, Ty<'_>> = Vec::new_in(frame);
        let mut tys: FVec<'_, Ty<'_>> = Vec::new_in(frame);
        if let Some(drop_impl) = hir_ty::drop::destructor(db, adt) {
            let args = GenericArgs::identity_for_item(interner, adt.into());
            let dangling = may_dangle(frame, db, drop_impl, args.len());
            for (index, arg) in args.iter().enumerate() {
                let dangles = dangling.get(index).copied().unwrap_or(false);
                match arg.kind() {
                    GenericArgKind::Type(arg) if dangles => tys.push(arg),
                    GenericArgKind::Type(arg) => whole.push(arg),
                    GenericArgKind::Lifetime(region) if !dangles => regions.push(region),
                    _ => (),
                }
            }
        }
        let mut fields = |variant: hir_def::VariantId| {
            for (_, field) in db.field_types(variant).iter() {
                let field = field.ty().instantiate_identity().skip_norm_wip();
                take_apart(db, interner, field, &mut regions, &mut whole, &mut tys);
            }
        };
        match adt {
            AdtId::StructId(it) => fields(it.into()),
            AdtId::UnionId(_) => (),
            AdtId::EnumId(it) => {
                for &(variant, _) in it.enum_variants(db).variants.values() {
                    fields(variant.into());
                }
            }
        }
        let mut seen = mem::set(frame);
        tys.retain(|it| seen.insert(*it));
        DropConstraint {
            regions: mem::owned(regions.iter().map(|it| it.store())),
            whole: mem::owned(whole.iter().map(|it| it.store())),
            tys: mem::owned(tys.iter().map(|it| it.store())),
        }
    })
}

/// An ADT that holds itself by value has no size and no answer.
fn drop_constraint_cycle(_db: &Db, _: blaze_query::db::Id, _adt: AdtId) -> DropConstraint {
    DropConstraint::default()
}

/// Takes `ty`, a field's type or part of one, apart for [`drop_constraint`].
fn take_apart<'db>(
    db: &'db Db,
    interner: DbInterner<'db>,
    ty: Ty<'db>,
    regions: &mut FVec<'_, Region<'db>>,
    whole: &mut FVec<'_, Ty<'db>>,
    tys: &mut FVec<'_, Ty<'db>>,
) {
    match ty.kind() {
        TyKind::Adt(def, _) if def.is_manually_drop() => (),
        TyKind::Adt(def, args) if def.is_box() || def.is_phantom_data() => {
            if let Some(GenericArgKind::Type(inner)) = args.iter().next().map(|it| it.kind()) {
                tys.push(inner);
            }
        }
        TyKind::Adt(def, args) => {
            let constraint = drop_constraint(db, def.def_id());
            let instantiate = |ty: &StoredTy| {
                rustc_type_ir::EarlyBinder::bind(interner, ty.as_ref())
                    .instantiate(interner, args)
                    .skip_norm_wip()
            };
            regions.extend(constraint.regions.iter().map(|it| {
                rustc_type_ir::EarlyBinder::bind(interner, it.as_ref())
                    .instantiate(interner, args)
                    .skip_norm_wip()
            }));
            whole.extend(constraint.whole.iter().map(instantiate));
            tys.extend(constraint.tys.iter().map(instantiate));
        }
        TyKind::Array(inner, _) | TyKind::Slice(inner) => {
            take_apart(db, interner, inner, regions, whole, tys)
        }
        TyKind::Tuple(parts) => {
            for it in parts.iter() {
                take_apart(db, interner, it, regions, whole, tys);
            }
        }
        TyKind::Dynamic(..) => whole.push(ty),
        TyKind::Param(_)
        | TyKind::Alias(_, _)
        | TyKind::Closure(..)
        | TyKind::CoroutineClosure(..)
        | TyKind::Coroutine(..) => tys.push(ty),
        _ => (),
    }
}

/// Per generic argument of the type a `Drop` impl is for, whether the impl says
/// `#[may_dangle]` of it: that dropping does not use it.
fn may_dangle<'f>(
    frame: &'f Frame,
    db: &Db,
    drop_impl: hir_def::ImplId,
    count: usize,
) -> FVec<'f, bool> {
    let (lifetimes, types) = AttrFlags::query_generic_params(db, drop_impl.into());
    let self_ty = db.impl_self_ty(drop_impl).skip_binder();
    let TyKind::Adt(_, args) = self_ty.kind() else { return filled(frame, count, false) };
    mem::collect(
        frame,
        args.iter().map(|arg| match arg.kind() {
            GenericArgKind::Type(ty) => match ty.kind() {
                TyKind::Param(param) => types
                    .get(param.id.local_id())
                    .is_some_and(|it| it.contains(AttrFlags::MAY_DANGLE)),
                _ => false,
            },
            GenericArgKind::Lifetime(region) => match region.kind() {
                rustc_type_ir::RegionKind::ReEarlyParam(early) => lifetimes
                    .get(early.id.local_id)
                    .is_some_and(|it| it.contains(AttrFlags::MAY_DANGLE)),
                _ => false,
            },
            GenericArgKind::Const(_) => false,
        }),
    )
}

/// The regions of `ty` a value of it can hold on to, as rustc's liveness `FreeRegionsVisitor`
/// has them: an alias known to outlive a region holds only that one, and none when it
/// outlives `'static`.
fn live_regions_of<'db>(
    ctx: &Ctx<'_, 'db>,
    regions: &Regions<'_, 'db>,
    ty: Ty<'db>,
    out: &mut FVec<'_, Rv>,
) {
    struct Visit<'c, 'a, 'r, 'o, 'db> {
        ctx: &'c Ctx<'a, 'db>,
        regions: &'c Regions<'r, 'db>,
        out: &'c mut FVec<'o, Rv>,
    }
    impl<'db> rustc_type_ir::TypeVisitor<DbInterner<'db>> for Visit<'_, '_, '_, '_, 'db> {
        type Result = ();
        fn visit_region(&mut self, region: Region<'db>) {
            if let Some(rv) = rv_of(region) {
                self.out.push(rv);
            }
        }
        fn visit_ty(&mut self, ty: Ty<'db>) {
            if let TyKind::Alias(_, alias) = ty.kind() {
                match alias_bound(self.ctx, self.regions, ty, alias) {
                    Some(None) => return,
                    Some(Some(rv)) => {
                        self.out.push(rv);
                        return;
                    }
                    None => (),
                }
                // What an `impl Trait` does not capture need not be live.
                if let rustc_type_ir::AliasTyKind::Opaque { def_id } = alias.kind
                    && let Some(captured) = super::regions::opaque_captures(
                        self.ctx.frame,
                        self.ctx.db,
                        def_id,
                        alias.args.len(),
                    )
                {
                    for (arg, captured) in alias.args.iter().zip(captured) {
                        if captured {
                            rustc_type_ir::TypeVisitable::visit_with(&arg, self);
                        }
                    }
                    return;
                }
            }
            rustc_type_ir::TypeSuperVisitable::super_visit_with(&ty, self)
        }
    }
    rustc_type_ir::TypeVisitable::visit_with(&ty, &mut Visit { ctx, regions, out });
}

/// [`live_regions_of`] `ty`.
pub(super) fn live_regions<'a, 'db>(
    ctx: &Ctx<'a, 'db>,
    regions: &Regions<'_, 'db>,
    ty: Ty<'db>,
) -> FVec<'a, Rv> {
    let mut out = Vec::new_in(ctx.frame);
    live_regions_of(ctx, regions, ty, &mut out);
    out
}

/// The one region the bounds of the alias `ty` say it outlives: `Some(None)` for `'static`,
/// `None` when they name none or several, or when `ty` is under a binder of its own type,
/// `fn(T::Gat<'a, '_>)`, whose bounds rustc does not match.
fn alias_bound<'db>(
    ctx: &Ctx<'_, 'db>,
    regions: &Regions<'_, 'db>,
    ty: Ty<'db>,
    alias: rustc_type_ir::AliasTy<DbInterner<'db>>,
) -> Option<Option<Rv>> {
    if rustc_type_ir::TypeVisitableExt::has_escaping_bound_vars(&ty) {
        return None;
    }
    let interner = ctx.infcx.interner;
    let erase = |ty: Ty<'db>| {
        rustc_type_ir::fold_regions(interner, ty, |_, _| Region::new_static(interner))
    };
    let erased = erase(ty);
    let item_bounds = mem::collect(
        ctx.frame,
        rustc_type_ir::Interner::item_bounds(interner, super::regions::alias_def_id(alias.kind))
            .iter_instantiated(interner, alias.args)
            .map(|it| it.skip_norm_wip()),
    );
    let mut found: Option<Option<Rv>> = None;
    for clause in item_bounds.iter().copied().chain(ctx.env.caller_bounds()) {
        let ClauseKind::TypeOutlives(OutlivesClause(bounded, region)) =
            clause.kind().skip_binder()
        else {
            continue;
        };
        let same = erase(bounded) == erased;
        if !same && normalize_erased(ctx, erase(bounded)) != Some(erased) {
            continue;
        }
        let bound = match region.kind() {
            RegionKind::ReStatic => return Some(None),
            // `for<'a> T: 'a` is `T: 'static`; `for<'a, 'b> T::Changes<'a, 'b>: 'b` that the
            // alias outlives what it has where the bound names `'b`.
            RegionKind::ReBound(_, bound) => {
                let regions_in = |ty: Ty<'db>| {
                    let mut found: FVec<'_, Region<'db>> = Vec::new_in(ctx.frame);
                    rustc_type_ir::fold_regions(interner, ty, |region, _| {
                        found.push(region);
                        region
                    });
                    found
                };
                let at = regions_in(bounded).iter().position(|it| {
                    matches!(it.kind(), RegionKind::ReBound(_, other) if other.var == bound.var)
                });
                match at {
                    None => return Some(None),
                    Some(at) if same => regions_in(ty).get(at).copied().and_then(rv_of),
                    Some(_) => None,
                }
            }
            RegionKind::ReVar(_) => rv_of(region),
            RegionKind::ReEarlyParam(early) => regions.early.get(&early.index).copied(),
            _ => None,
        };
        let Some(bound) = bound else { continue };
        match found {
            None => found = Some(Some(bound)),
            Some(Some(it)) if it == bound => (),
            Some(_) => return None,
        }
    }
    found
}

/// `ty`, its regions all `'static`, normalized as far as the solver goes: rustc normalizes the
/// outlives clauses of the parameter environment before alias liveness matches them.
pub(super) fn normalize_erased<'db>(ctx: &Ctx<'_, 'db>, ty: Ty<'db>) -> Option<Ty<'db>> {
    use hir_ty::next_solver::{infer::traits::ObligationCause, obligation_ctxt::ObligationCtxt};
    if !rustc_type_ir::TypeVisitableExt::has_aliases(&ty) {
        return None;
    }
    let infcx = &ctx.infcx;
    infcx.probe(|_| {
        let ocx = ObligationCtxt::new(infcx);
        let normalized = ocx.deeply_normalize(&ObligationCause::dummy(), ctx.env, ty).ok()?;
        let interner = infcx.interner;
        Some(rustc_type_ir::fold_regions(
            interner,
            infcx.deeply_resolve_ignoring_regions(normalized),
            |_, _| Region::new_static(interner),
        ))
    })
}

#[cfg(test)]
mod tests {
    use blaze_ids::rng::SplitMix64;
    use blaze_mem::{Tag, frame};

    use super::{IntervalSet, least_rank};

    #[test]
    fn the_least_rank_per_run_is_the_least_per_point() {
        let mut rng = SplitMix64::new(0x9e37_79b9_7f4a_7c15);
        for _ in 0..2000 {
            frame::task(Tag::Mir, |frame| {
                // Blocks of 1 to 6 points, laid out one after another.
                let blocks: Vec<usize> =
                    (0..1 + rng.index(8)).map(|_| 1 + rng.index(6)).collect();
                let starts: Vec<usize> = blocks
                    .iter()
                    .scan(0, |at, &len| {
                        let start = *at;
                        *at += len;
                        Some(start)
                    })
                    .collect();
                let count: usize = blocks.iter().sum();
                let block_of = |point: usize| starts.partition_point(|&it| it <= point) - 1;
                // Some blocks walked in a shuffled order, each from its end; the rest unreached.
                let mut order: Vec<usize> = (0..blocks.len()).collect();
                rng.shuffle(&mut order);
                order.truncate(1 + rng.index(blocks.len()));
                let mut rank = vec![usize::MAX; count];
                let mut next = 0;
                for &block in &order {
                    for point in (starts[block]..starts[block] + blocks[block]).rev() {
                        rank[point] = next;
                        next += 1;
                    }
                }
                let mut points = IntervalSet::new(frame);
                for point in 0..count {
                    if rng.index(3) == 0 {
                        points.insert(point);
                    }
                }
                let want = points.iter().map(|point| rank[point]).min().unwrap_or(usize::MAX);
                let block_end = |point| {
                    let block = block_of(point);
                    starts[block] + blocks[block] - 1
                };
                assert_eq!(
                    least_rank(&points, &rank, count, block_end),
                    want,
                    "blocks {blocks:?}, order {order:?}, points {:?}",
                    points.iter().collect::<Vec<_>>()
                );
            });
        }
    }
}
