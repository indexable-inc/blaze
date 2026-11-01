//! Move paths, and where each may be uninitialized or has ever been initialized: rustc's
//! `MoveData` with `MaybeUninitializedPlaces` and `EverInitializedPlaces`.
//!
//! A move path is a local, or a field, variant, element of an array of known length or the
//! contents of a `Box` of another path. Every local has one; a part has one when the body
//! moves out of it, assigns it or drops it. What is behind any other pointer, at an index
//! known only when the body runs, or inside a value whose type implements `Drop`, has none:
//! it cannot be moved out of, and the nearest path above stands for it. A field of a union is
//! the union.
//!
//! The paths are numbered in preorder, each local's tree in local order and a path's children
//! in the order they were made, so a path and everything below it are one range of ids: a
//! move or an init sets or clears a range of bits a word at a time, where rustc's
//! `on_all_children_bits` walks the tree a bit at a time, and whether anything below a path is
//! uninitialized is one scan of the words.

use std::ops::Range;

use blaze_hash::HashMap;
use rustc_type_ir::inherent::IntoKind;

use hir_ty::{
    consteval::try_const_usize,
    mir::{
        AsmOperand, BasicBlockId, LocalId, MirBody, PlaceElem, PlaceTy, ProjectionElem, Rvalue,
        StatementKind, StoredPlace, TerminatorKind,
    },
    next_solver::TyKind,
};

use super::{
    Ctx,
    bitset::{BitMatrix, BitSet, Transfer},
    cfg::{Cfg, Location, index},
    dataflow::{GenKill, forward},
    relation::{Csr, Facts, Relation},
};

pub(crate) type Mpi = usize;

const NONE: u32 = u32::MAX;

/// What a place is in terms of move paths.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Lookup {
    Exact(Mpi),
    /// The place has no path of its own; this is the nearest one above it.
    Parent(Mpi),
}

impl Lookup {
    pub(crate) fn path(self) -> Mpi {
        match self {
            Lookup::Exact(it) | Lookup::Parent(it) => it,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct MoveOut {
    pub(crate) path: Mpi,
    pub(crate) location: Location,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum InitKind {
    Deep,
    /// The destination of a call, initialized only if the call returns.
    NonPanicPathOnly,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Init {
    pub(crate) path: Mpi,
    /// `None` for an argument, initialized before the body starts.
    pub(crate) location: Option<Location>,
    pub(crate) kind: InitKind,
}

const _: () = assert!(size_of::<MoveOut>() <= 24);
const _: () = assert!(size_of::<Init>() <= 40);

/// The kinds of [`MoveData`]'s facts: the moves and the inits at a point, the moves of a path.
const MOVE_AT: u8 = 0;
const INIT_AT: u8 = 1;
const MOVE_OF: u8 = 2;

/// The move paths as columns by preorder id: 4 bytes of local, 4 of parent and 4 of subtree
/// end per path, the projections one CSR. Finding a path's child is one hash probe.
pub(crate) struct MoveData {
    local: Vec<LocalId>,
    parent: Vec<u32>,
    /// One past the last id below each path.
    end: Vec<u32>,
    projections: Csr<PlaceElem>,
    /// Each local's path, by its index.
    locals: Vec<u32>,
    /// The path below a path by one step.
    steps: HashMap<(u32, PlaceElem), u32>,
    pub(crate) moves: Vec<MoveOut>,
    pub(crate) inits: Vec<Init>,
    /// The moves and inits at each point and of each path, each in the order it was found.
    facts: Relation,
}

/// The move paths while they are gathered: a tree linked by `u32` ids in the order the paths
/// are made, the locals' first. Frozen into [`MoveData`]'s preorder columns at the end.
struct Tree {
    local: Vec<LocalId>,
    parent: Vec<u32>,
    depth: Vec<u32>,
    first_child: Vec<u32>,
    last_child: Vec<u32>,
    next_sibling: Vec<u32>,
    /// The step from its parent of each path but a local's, by id less the number of locals.
    step: Vec<PlaceElem>,
    roots: usize,
    steps: HashMap<(u32, PlaceElem), u32>,
    moves: Vec<MoveOut>,
    inits: Vec<Init>,
}

impl MoveData {
    pub(crate) fn gather<'db>(ctx: &Ctx<'_, 'db>, body: &MirBody<'db>, cfg: &Cfg<'_>) -> Self {
        let roots = body.locals.len();
        let mut data = Tree {
            local: Vec::with_capacity(roots * 2),
            parent: Vec::with_capacity(roots * 2),
            depth: Vec::with_capacity(roots * 2),
            first_child: Vec::with_capacity(roots * 2),
            last_child: Vec::with_capacity(roots * 2),
            next_sibling: Vec::with_capacity(roots * 2),
            step: Vec::with_capacity(roots),
            roots,
            steps: HashMap::default(),
            moves: Vec::with_capacity(cfg.points / 2),
            inits: Vec::with_capacity(cfg.points / 2),
        };
        for (local, _) in body.locals.iter() {
            data.add(local, NONE, 0);
        }
        for &local in &body.param_locals {
            let path = local.into_raw().into_u32() as usize;
            data.inits.push(Init { path, location: None, kind: InitKind::Deep });
        }
        for &block in &cfg.order {
            let data_block = &body.basic_blocks[block];
            for (statement, it) in data_block.statements.iter().enumerate() {
                let location = Location { block, statement };
                let compares =
                    matches!(&it.kind, StatementKind::Assign(_, rvalue) if super::compares(rvalue));
                it.kind.for_each_operand(&mut |operand| {
                    if !compares
                        && let Some(place) = ctx.moved_place(body, operand)
                        && !ctx.moves_out_of_guard(
                            place.local,
                            place.projection.as_slice(),
                            operand.span().unwrap_or(it.span),
                        )
                    {
                        data.gather_move(ctx, body, place, location);
                    }
                });
                match &it.kind {
                    StatementKind::Assign(place, rvalue) => {
                        if let Rvalue::Discriminant(moved) = rvalue
                            && ctx.cast_moves(body, moved, it.span)
                        {
                            data.gather_move(ctx, body, moved, location);
                        }
                        data.gather_init(ctx, body, place, location, InitKind::Deep)
                    }
                    StatementKind::StorageDead(local) => {
                        if !body.param_locals.contains(local) {
                            let path = local.into_raw().into_u32() as usize;
                            data.moves.push(MoveOut { path, location });
                        }
                    }
                    StatementKind::Deinit(place) => data.gather_move(ctx, body, place, location),
                    StatementKind::InlineAsm(asm) => {
                        for operand in &asm.operands {
                            if let AsmOperand::Reg { output: Some(output), .. } = operand {
                                data.gather_init(ctx, body, output, location, InitKind::Deep);
                            }
                        }
                    }
                    _ => (),
                }
            }
            let Some(terminator) = &data_block.terminator else { continue };
            let location = Location { block, statement: data_block.statements.len() };
            terminator.kind.for_each_operand(&mut |operand| {
                if let Some(place) = ctx.moved_place(body, operand)
                    && !ctx.moves_out_of_guard(
                        place.local,
                        place.projection.as_slice(),
                        operand.span().unwrap_or(terminator.span),
                    )
                {
                    data.gather_move(ctx, body, place, location);
                }
            });
            match &terminator.kind {
                TerminatorKind::Drop { place, .. } => data.gather_move(ctx, body, place, location),
                TerminatorKind::DropAndReplace { place, .. } => {
                    data.gather_init(ctx, body, place, location, InitKind::Deep)
                }
                TerminatorKind::Call { destination, .. } => {
                    data.gather_init(ctx, body, destination, location, InitKind::NonPanicPathOnly)
                }
                TerminatorKind::Yield { resume_arg, .. } => {
                    data.gather_init(ctx, body, resume_arg, location, InitKind::Deep)
                }
                _ => (),
            }
        }
        data.freeze(cfg)
    }

    pub(crate) fn len(&self) -> usize {
        self.local.len()
    }

    pub(crate) fn local(&self, path: Mpi) -> LocalId {
        self.local[path]
    }

    pub(crate) fn parent(&self, path: Mpi) -> Option<Mpi> {
        let parent = self.parent[path];
        (parent != NONE).then_some(parent as Mpi)
    }

    /// The projection of the place `path` is, from its local.
    pub(crate) fn projection(&self, path: Mpi) -> &[PlaceElem] {
        self.projections.of(path)
    }

    /// `path` and every path below it.
    pub(crate) fn subtree(&self, path: Mpi) -> Range<usize> {
        path..self.end[path] as usize
    }

    /// The paths right below `path`, in the order they were made.
    pub(crate) fn children(&self, path: Mpi) -> impl Iterator<Item = Mpi> + '_ {
        let end = self.end[path] as usize;
        let mut at = path + 1;
        std::iter::from_fn(move || {
            (at < end).then(|| {
                let child = at;
                at = self.end[child] as usize;
                child
            })
        })
    }

    /// The first path at or below `path`, in preorder, that is in `set`: rustc's
    /// `find_in_move_path_or_its_descendants`.
    pub(crate) fn first_in(&self, path: Mpi, set: &BitSet) -> Option<Mpi> {
        set.first_in(self.subtree(path))
    }

    pub(crate) fn is_ancestor_or_self(&self, ancestor: Mpi, path: Mpi) -> bool {
        self.subtree(ancestor).contains(&path)
    }

    /// The moves out at `point`, in the order they happen.
    pub(crate) fn moves_at(&self, point: usize) -> &[u32] {
        self.facts.get(MOVE_AT, point)
    }

    /// The inits at `point`, in the order they happen.
    pub(crate) fn inits_at(&self, point: usize) -> &[u32] {
        self.facts.get(INIT_AT, point)
    }

    /// The moves out of `path`.
    pub(crate) fn moves_of(&self, path: Mpi) -> &[u32] {
        self.facts.get(MOVE_OF, path)
    }

    /// The path of `place` if it has been made, or the nearest one above it.
    pub(crate) fn find<'db>(
        &self,
        ctx: &Ctx<'_, 'db>,
        body: &MirBody<'db>,
        local: LocalId,
        projection: &[PlaceElem],
    ) -> Lookup {
        let mut path = self.locals[local.into_raw().into_u32() as usize];
        let mut ty = PlaceTy::from_ty(body.locals[local].ty.as_ref());
        for elem in projection {
            let Some(step) = ctx.old_trackable(ty, elem) else { return Lookup::Parent(path as Mpi) };
            match self.steps.get(&(path, step)) {
                Some(&it) => path = it,
                None => return Lookup::Parent(path as Mpi),
            }
            ty = ctx.project(ty, elem);
        }
        Lookup::Exact(path as Mpi)
    }

    pub(crate) fn local_path(&self, local: LocalId) -> Mpi {
        self.locals[local.into_raw().into_u32() as usize] as Mpi
    }
}

impl Tree {
    fn add(&mut self, local: LocalId, parent: u32, depth: u32) -> u32 {
        let id = self.local.len() as u32;
        self.local.push(local);
        self.parent.push(parent);
        self.depth.push(depth);
        self.first_child.push(NONE);
        self.last_child.push(NONE);
        self.next_sibling.push(NONE);
        if parent != NONE {
            let parent = parent as usize;
            match self.last_child[parent] {
                NONE => self.first_child[parent] = id,
                last => self.next_sibling[last as usize] = id,
            }
            self.last_child[parent] = id;
        }
        id
    }

    fn gather_move<'db>(
        &mut self,
        ctx: &Ctx<'_, 'db>,
        body: &MirBody<'db>,
        place: &StoredPlace,
        location: Location,
    ) {
        // A move of a subslice of an array moves each element of it.
        if let Some((ProjectionElem::Subslice { from, to }, base)) =
            place.projection.as_slice().split_last()
            && let Lookup::Exact(array) = self.create(ctx, body, place.local, base)
            && let TyKind::Array(_, len) = ctx.place_ty(body, place.local, base).ty.kind()
            && let Some(len) = try_const_usize(ctx.db, len)
        {
            for offset in *from..(len as u64).saturating_sub(*to) {
                let path = self.child(
                    array as u32,
                    place.local,
                    ProjectionElem::ConstantIndex {
                        offset,
                        min_length: offset + 1,
                        from_end: false,
                    },
                );
                self.moves.push(MoveOut { path: path as Mpi, location });
            }
            return;
        }
        let projection = place.projection.as_slice();
        match self.create(ctx, body, place.local, projection) {
            Lookup::Exact(path) => self.moves.push(MoveOut { path, location }),
            // A move out of a field of a union moves the union, as long as nothing else on the
            // way makes it an error.
            Lookup::Parent(path) => {
                let depth = self.depth[path] as usize;
                let base_ty = ctx.place_ty(body, place.local, &projection[..depth]).ty;
                if matches!(projection[depth], ProjectionElem::Field(_))
                    && matches!(base_ty.kind(), TyKind::Adt(def, _) if def.is_union())
                    && projection[depth + 1..]
                        .iter()
                        .all(|it| matches!(it, ProjectionElem::Field(_)))
                {
                    self.moves.push(MoveOut { path, location });
                }
            }
        }
    }

    fn child(&mut self, parent: u32, local: LocalId, step: PlaceElem) -> u32 {
        if let Some(&it) = self.steps.get(&(parent, step)) {
            return it;
        }
        let id = self.add(local, parent, self.depth[parent as usize] + 1);
        self.step.push(step);
        self.steps.insert((parent, step), id);
        id
    }

    fn gather_init<'db>(
        &mut self,
        ctx: &Ctx<'_, 'db>,
        body: &MirBody<'db>,
        place: &StoredPlace,
        location: Location,
        kind: InitKind,
    ) {
        let projection = place.projection.as_slice();
        let path = match self.create(ctx, body, place.local, projection) {
            Lookup::Exact(path) => path,
            // Assigning a field of a union initializes the union.
            Lookup::Parent(path) if self.is_union_field(ctx, body, place, path) => path,
            Lookup::Parent(_) => return,
        };
        self.inits.push(Init { path, location: Some(location), kind });
    }

    fn is_union_field<'db>(
        &self,
        ctx: &Ctx<'_, 'db>,
        body: &MirBody<'db>,
        place: &StoredPlace,
        parent: Mpi,
    ) -> bool {
        let projection = place.projection.as_slice();
        let depth = self.depth[parent] as usize;
        if projection.len() != depth + 1 || !matches!(projection[depth], ProjectionElem::Field(_)) {
            return false;
        }
        let ty = ctx.place_ty(body, place.local, &projection[..depth]).ty;
        matches!(ty.kind(), TyKind::Adt(def, _) if def.is_union())
    }

    /// The path of `local.projection`, made if it can have one.
    fn create<'db>(
        &mut self,
        ctx: &Ctx<'_, 'db>,
        body: &MirBody<'db>,
        local: LocalId,
        projection: &[PlaceElem],
    ) -> Lookup {
        let mut path = local.into_raw().into_u32();
        let mut ty = PlaceTy::from_ty(body.locals[local].ty.as_ref());
        for elem in projection {
            let Some(step) = ctx.old_trackable(ty, elem) else { return Lookup::Parent(path as Mpi) };
            path = self.child(path, local, step);
            ty = ctx.project(ty, elem);
        }
        Lookup::Exact(path as Mpi)
    }

    /// The paths renumbered in preorder, without a stack: down to the first child, else on
    /// to the next sibling, else back up.
    fn freeze(self, cfg: &Cfg<'_>) -> MoveData {
        let count = self.local.len();
        let mut order: Vec<u32> = Vec::with_capacity(count);
        let mut renumbered = vec![NONE; count];
        for root in 0..self.roots {
            let mut at = root;
            'walk: loop {
                renumbered[at] = order.len() as u32;
                order.push(at as u32);
                if self.first_child[at] != NONE {
                    at = self.first_child[at] as usize;
                    continue;
                }
                while at != root {
                    if self.next_sibling[at] != NONE {
                        at = self.next_sibling[at] as usize;
                        continue 'walk;
                    }
                    at = self.parent[at] as usize;
                }
                break;
            }
        }
        let depths: usize = self.depth.iter().map(|&it| it as usize).sum();
        let mut local = Vec::with_capacity(count);
        let mut parent = Vec::with_capacity(count);
        let mut end: Vec<u32> = (1..=count as u32).collect();
        let mut projections = Csr::with_capacity(count, depths);
        for &old in &order {
            let old = old as usize;
            local.push(self.local[old]);
            let up = self.parent[old];
            if up == NONE {
                parent.push(NONE);
            } else {
                let up = renumbered[up as usize];
                parent.push(up);
                projections.push_from(up as usize);
                projections.push(self.step[old - self.roots]);
            }
            projections.end_node();
        }
        for path in (0..count).rev() {
            if parent[path] != NONE {
                let up = parent[path] as usize;
                end[up] = end[up].max(end[path]);
            }
        }
        let mut steps = HashMap::with_capacity_and_hasher(count - self.roots, Default::default());
        for old in self.roots..count {
            let up = renumbered[self.parent[old] as usize];
            steps.insert((up, self.step[old - self.roots]), renumbered[old]);
        }
        let locals = renumbered[..self.roots].to_vec();
        let mut moves = self.moves;
        for it in &mut moves {
            it.path = renumbered[it.path] as Mpi;
        }
        let mut inits = self.inits;
        for it in &mut inits {
            it.path = renumbered[it.path] as Mpi;
        }
        let mut facts = Facts::with_capacity(moves.len() * 2 + inits.len());
        for (index, it) in moves.iter().enumerate() {
            facts.push(MOVE_AT, cfg.point(it.location), index);
            facts.push(MOVE_OF, it.path, index);
        }
        for (index, it) in inits.iter().enumerate() {
            if let Some(location) = it.location {
                facts.push(INIT_AT, cfg.point(location), index);
            }
        }
        let facts = facts.freeze(&[cfg.points, cfg.points, count]);
        MoveData { local, parent, end, projections, locals, steps, moves, inits, facts }
    }
}

impl<'db> Ctx<'_, 'db> {
    /// The step of a move path that `elem` of a place of type `ty` is, if it has one.
    pub(crate) fn old_trackable(&self, ty: PlaceTy<'db>, elem: &PlaceElem) -> Option<PlaceElem> {
        match elem {
            ProjectionElem::Deref => ty.ty.is_box().then_some(ProjectionElem::Deref),
            ProjectionElem::Field(_) => match ty.ty.kind() {
                TyKind::Adt(def, _) if def.is_union() => None,
                TyKind::Adt(def, _)
                    if ty.variant_id.is_none() && self.has_destructor(def.def_id()) =>
                {
                    None
                }
                TyKind::Adt(def, _)
                    if ty.variant_id.is_some() && self.has_destructor(def.def_id()) =>
                {
                    None
                }
                _ => Some(*elem),
            },
            ProjectionElem::Downcast(_) => match ty.ty.kind() {
                TyKind::Adt(def, _) if self.has_destructor(def.def_id()) => None,
                _ => Some(*elem),
            },
            ProjectionElem::Index(_) | ProjectionElem::Subslice { .. } => None,
            &ProjectionElem::ConstantIndex { offset, from_end, .. } => match ty.ty.kind() {
                TyKind::Array(_, len) => {
                    let len = try_const_usize(self.db, len)? as u64;
                    let offset = if from_end { len.checked_sub(offset)? } else { offset };
                    Some(ProjectionElem::ConstantIndex {
                        offset,
                        min_length: offset + 1,
                        from_end: false,
                    })
                }
                _ => None,
            },
        }
    }
}

/// Which paths may be uninitialized at the start of each block: rustc's
/// `MaybeUninitializedPlaces`. And per local, whether it may have been initialized since its
/// storage last ended: `EverInitializedPlaces`, of whole locals.
pub(crate) struct InitFlow {
    /// A row of move paths per block.
    pub(crate) uninit_entry: BitMatrix,
    /// A row of locals per block.
    pub(crate) ever_entry: BitMatrix,
}

impl InitFlow {
    pub(crate) fn compute(body: &MirBody<'_>, cfg: &Cfg<'_>, moves: &MoveData) -> Self {
        let blocks = body.basic_blocks.len();
        let paths = moves.len();
        let locals = body.locals.len();
        let mut uninit = GenKill::new(blocks, paths);
        let mut ever = GenKill::new(blocks, locals);
        // A call's return: what its destination being initialized does on that edge only, a
        // row per reachable call rather than per block.
        let returning = |block: BasicBlockId| match body.basic_blocks[block].terminator.as_ref() {
            Some(it) => match it.kind {
                TerminatorKind::Call { target: Some(target), .. } => Some(target),
                _ => None,
            },
            None => None,
        };
        let calls = cfg.order.iter().filter(|&&block| returning(block).is_some()).count();
        let mut returned_uninit = GenKill::new(calls, paths);
        let mut returned_ever = GenKill::new(calls, locals);
        // Per block, the block its call returns to and the call's row.
        let mut returns_to: Vec<Option<(BasicBlockId, usize)>> = vec![None; blocks];
        let mut call = 0;
        for &block in &cfg.order {
            let data = &body.basic_blocks[block];
            let (mut uninit, mut ever) = (uninit.row(index(block)), ever.row(index(block)));
            for statement in 0..=data.statements.len() {
                apply(body, moves, cfg, Location { block, statement }, &mut uninit, &mut ever);
            }
            if let Some(target) = returning(block) {
                returns_to[index(block)] = Some((target, call));
                let location = Location { block, statement: data.statements.len() };
                call_returned(
                    moves,
                    cfg,
                    location,
                    &mut returned_uninit.row(call),
                    &mut returned_ever.row(call),
                );
                call += 1;
            }
        }
        let mut uninit_start = BitSet::filled(paths);
        let mut ever_start = BitSet::new(locals);
        for init in moves.inits.iter().filter(|it| it.location.is_none()) {
            uninit_start.kill_range(moves.subtree(init.path));
            ever_start.insert(moves.local(init.path).into_raw().into_u32() as usize);
        }
        // A block with no terminator yet leads nowhere.
        let ends = |from: BasicBlockId, state: &mut [u64]| {
            if body.basic_blocks[from].terminator.is_none() {
                state.fill(0);
            }
        };
        let uninit_entry =
            forward(cfg, &uninit, body.start_block, &uninit_start, |from, to, state| {
                if let Some((target, call)) = returns_to[index(from)]
                    && target == to
                {
                    returned_uninit.apply(call, state);
                }
                ends(from, state);
            });
        let ever_entry = forward(cfg, &ever, body.start_block, &ever_start, |from, to, state| {
            if let Some((target, call)) = returns_to[index(from)]
                && target == to
            {
                returned_ever.apply(call, state);
            }
            ends(from, state);
        });
        InitFlow { uninit_entry, ever_entry }
    }
}

/// The effect of the statement or terminator at `location` on what may be uninitialized and
/// what has ever been initialized, the return of a call aside: on the sets as the walk goes,
/// or composed into a block's transfer function.
pub(crate) fn apply(
    body: &MirBody<'_>,
    moves: &MoveData,
    cfg: &Cfg<'_>,
    location: Location,
    uninit: &mut impl Transfer,
    ever: &mut impl Transfer,
) {
    let point = cfg.point(location);
    for &index in moves.moves_at(point) {
        uninit.gen_range(moves.subtree(moves.moves[index as usize].path));
    }
    for &index in moves.inits_at(point) {
        let init = moves.inits[index as usize];
        if init.kind == InitKind::Deep {
            uninit.kill_range(moves.subtree(init.path));
            if moves.parent(init.path).is_none() {
                ever.gen_(moves.local(init.path).into_raw().into_u32() as usize);
            }
        }
    }
    let data = &body.basic_blocks[location.block];
    if let Some(statement) = data.statements.get(location.statement)
        && let StatementKind::StorageDead(local) = statement.kind
    {
        ever.kill(local.into_raw().into_u32() as usize);
    }
}

/// What the call at `location` returning does: its destination is initialized.
fn call_returned(
    moves: &MoveData,
    cfg: &Cfg<'_>,
    location: Location,
    uninit: &mut impl Transfer,
    ever: &mut impl Transfer,
) {
    for &index in moves.inits_at(cfg.point(location)) {
        let init = moves.inits[index as usize];
        if init.kind == InitKind::NonPanicPathOnly {
            uninit.kill_range(moves.subtree(init.path));
            if moves.parent(init.path).is_none() {
                ever.gen_(moves.local(init.path).into_raw().into_u32() as usize);
            }
        }
    }
}
