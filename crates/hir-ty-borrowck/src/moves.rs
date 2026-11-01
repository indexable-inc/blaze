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
//!
//! The moves and inits come from the decode's columns ([`Decoded`]), point by point in reverse
//! postorder, each point's moves before its `StorageDead` before its writes: the order rustc
//! numbers its move outs in, which E0382's buffering keys on.

use std::{cell::RefCell, ops::Range};

use rustc_type_ir::inherent::IntoKind;

use hir_ty::{
    consteval::try_const_usize,
    mir::{BasicBlockId, LocalId, MirBody, PlaceElem, PlaceTy, ProjectionElem},
    next_solver::TyKind,
};

use super::{
    Ctx,
    bitset::{BitSet, Transfer, bits, filled_bits},
    cfg::{Cfg, Location, index},
    dataflow::forward,
    decode::{Decoded, MoveKind, Moved, NONE, WriteKind, Written, local, raw},
    mem::{self, FMap, FVec, Frame, filled},
    places::PlaceId,
    relation::{Csr, Facts, Relation},
    rows::{Edge, Effects, Rows},
};

pub(super) type Mpi = usize;

/// What a place is in terms of move paths.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Lookup {
    Exact(Mpi),
    /// The place has no path of its own; this is the nearest one above it.
    Parent(Mpi),
}

impl Lookup {
    pub(super) fn path(self) -> Mpi {
        match self {
            Lookup::Exact(it) | Lookup::Parent(it) => it,
        }
    }

    fn pack(self) -> u32 {
        match self {
            Lookup::Exact(it) => (it as u32) << 1,
            Lookup::Parent(it) => ((it as u32) << 1) | 1,
        }
    }

    fn unpack(it: u32) -> Self {
        match it & 1 {
            0 => Lookup::Exact((it >> 1) as Mpi),
            _ => Lookup::Parent((it >> 1) as Mpi),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct MoveOut {
    pub(super) path: Mpi,
    pub(super) location: Location,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum InitKind {
    Deep,
    /// The destination of a call, initialized only if the call returns.
    NonPanicPathOnly,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Init {
    pub(super) path: Mpi,
    /// `None` for an argument, initialized before the body starts.
    pub(super) location: Option<Location>,
    pub(super) kind: InitKind,
}

const _: () = assert!(size_of::<MoveOut>() <= 24);
const _: () = assert!(size_of::<Init>() <= 40);

/// The kinds of [`MoveData`]'s facts: the moves and the inits at a point, the moves of a path.
const MOVE_AT: u8 = 0;
const INIT_AT: u8 = 1;
const MOVE_OF: u8 = 2;

/// The move paths as columns by preorder id: 4 bytes of local, 4 of parent and 4 of subtree
/// end per path, the projections one CSR. Finding a path's child is one hash probe.
pub(super) struct MoveData<'f> {
    local: FVec<'f, LocalId>,
    parent: FVec<'f, u32>,
    /// One past the last id below each path.
    end: FVec<'f, u32>,
    projections: Csr<'f, PlaceElem>,
    /// Each local's path, by its index.
    locals: &'f [u32],
    /// The path below a path by one step.
    steps: FMap<'f, (u32, PlaceElem), u32>,
    /// What each place is, [`Lookup::pack`]ed, by [`PlaceId`]; [`NONE`] until asked.
    lookups: RefCell<FVec<'f, u32>>,
    pub(super) moves: FVec<'f, MoveOut>,
    pub(super) inits: FVec<'f, Init>,
    /// The moves and inits at each point and of each path, each in the order it was found.
    facts: Relation<'f>,
    /// By point, the local whose storage ends there; by block, where its call returns to and
    /// whether it has a terminator: the decode's.
    dead: &'f [u32],
    returns: &'f [u32],
    terminated: &'f [bool],
}

/// The move paths while they are gathered: a tree linked by `u32` ids in the order the paths
/// are made, the locals' first. Frozen into [`MoveData`]'s preorder columns at the end.
struct Tree<'f> {
    local: FVec<'f, LocalId>,
    parent: FVec<'f, u32>,
    depth: FVec<'f, u32>,
    first_child: FVec<'f, u32>,
    last_child: FVec<'f, u32>,
    next_sibling: FVec<'f, u32>,
    /// The step from its parent of each path but a local's, by id less the number of locals.
    step: FVec<'f, PlaceElem>,
    roots: usize,
    steps: FMap<'f, (u32, PlaceElem), u32>,
    /// What each place is, by [`PlaceId`], in the tree's ids.
    lookups: FVec<'f, u32>,
    moves: FVec<'f, MoveOut>,
    inits: FVec<'f, Init>,
}

impl<'f> MoveData<'f> {
    pub(super) fn gather<'db>(
        ctx: &Ctx<'f, 'db>,
        body: &MirBody<'db>,
        cfg: &Cfg<'_>,
        decoded: &Decoded<'f>,
    ) -> Self {
        let frame = ctx.frame;
        let roots = body.locals.len();
        let mut data = Tree {
            local: Vec::with_capacity_in(roots * 2, frame),
            parent: Vec::with_capacity_in(roots * 2, frame),
            depth: Vec::with_capacity_in(roots * 2, frame),
            first_child: Vec::with_capacity_in(roots * 2, frame),
            last_child: Vec::with_capacity_in(roots * 2, frame),
            next_sibling: Vec::with_capacity_in(roots * 2, frame),
            step: Vec::with_capacity_in(roots, frame),
            roots,
            steps: mem::map(frame),
            lookups: filled(frame, ctx.places.borrow().len(), NONE),
            moves: Vec::with_capacity_in(cfg.points / 2, frame),
            inits: Vec::with_capacity_in(cfg.points / 2, frame),
        };
        for (local, _) in body.locals.iter() {
            data.add(local, NONE, 0);
        }
        for &local in &body.param_locals {
            data.inits.push(Init { path: raw(local), location: None, kind: InitKind::Deep });
        }
        for &block in cfg.order.iter() {
            let start = cfg.block_start(block);
            for point in start..=cfg.block_end(body, block) {
                let location = Location { block, statement: point - start };
                for &moved in decoded.moves.of(point) {
                    data.gather_move(ctx, decoded, moved, location);
                }
                let dead = decoded.dead[point];
                if dead != NONE && !body.param_locals.contains(&local(dead)) {
                    data.moves.push(MoveOut { path: dead as Mpi, location });
                }
                for &written in decoded.writes.of(point) {
                    data.gather_init(ctx, decoded, written, location);
                }
            }
        }
        data.freeze(frame, cfg, decoded)
    }

    pub(super) fn len(&self) -> usize {
        self.local.len()
    }

    pub(super) fn local(&self, path: Mpi) -> LocalId {
        self.local[path]
    }

    pub(super) fn parent(&self, path: Mpi) -> Option<Mpi> {
        let parent = self.parent[path];
        (parent != NONE).then_some(parent as Mpi)
    }

    /// The projection of the place `path` is, from its local.
    pub(super) fn projection(&self, path: Mpi) -> &[PlaceElem] {
        self.projections.of(path)
    }

    /// `path` and every path below it.
    pub(super) fn subtree(&self, path: Mpi) -> Range<usize> {
        path..self.end[path] as usize
    }

    /// The paths right below `path`, in the order they were made.
    pub(super) fn children(&self, path: Mpi) -> impl Iterator<Item = Mpi> + '_ {
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
    pub(super) fn first_in(&self, path: Mpi, set: &BitSet<'_>) -> Option<Mpi> {
        set.first_in(self.subtree(path))
    }

    pub(super) fn is_ancestor_or_self(&self, ancestor: Mpi, path: Mpi) -> bool {
        self.subtree(ancestor).contains(&path)
    }

    /// The moves out at `point`, in the order they happen.
    pub(super) fn moves_at(&self, point: usize) -> &'f [u32] {
        self.facts.get(MOVE_AT, point)
    }

    /// The inits at `point`, in the order they happen.
    pub(super) fn inits_at(&self, point: usize) -> &'f [u32] {
        self.facts.get(INIT_AT, point)
    }

    /// The moves out of `path`.
    pub(super) fn moves_of(&self, path: Mpi) -> &'f [u32] {
        self.facts.get(MOVE_OF, path)
    }

    /// The path of `place` if it has been made, or the nearest one above it.
    pub(super) fn find<'db>(
        &self,
        ctx: &Ctx<'_, 'db>,
        _body: &MirBody<'db>,
        local: LocalId,
        projection: &[PlaceElem],
    ) -> Lookup {
        self.find_place(ctx, ctx.place(local, projection))
    }

    fn find_place(&self, ctx: &Ctx<'_, '_>, place: PlaceId) -> Lookup {
        if let Some(&known) = self.lookups.borrow().get(place as usize)
            && known != NONE
        {
            return Lookup::unpack(known);
        }
        let parent = ctx.places.borrow().parent(place);
        let found = match parent {
            None => Lookup::Exact(self.locals[place as usize] as Mpi),
            Some(parent) => match self.find_place(ctx, parent) {
                Lookup::Parent(path) => Lookup::Parent(path),
                Lookup::Exact(path) => {
                    match step_of(ctx, parent, place) {
                        Some(step) => match self.steps.get(&(path as u32, step)) {
                            Some(&child) => Lookup::Exact(child as Mpi),
                            None => Lookup::Parent(path),
                        },
                        None => Lookup::Parent(path),
                    }
                }
            },
        };
        let mut lookups = self.lookups.borrow_mut();
        if lookups.len() <= place as usize {
            lookups.resize(place as usize + 1, NONE);
        }
        lookups[place as usize] = found.pack();
        found
    }

    pub(super) fn local_path(&self, local: LocalId) -> Mpi {
        self.locals[raw(local)] as Mpi
    }
}

/// The step of a move path from `parent` that `place`, one element below it, is, if it has
/// one.
fn step_of(ctx: &Ctx<'_, '_>, parent: PlaceId, place: PlaceId) -> Option<PlaceElem> {
    let (ty, elem) = {
        let places = ctx.places.borrow();
        (places.ty(parent), places.elem(place).copied()?)
    };
    ctx.trackable(ty, &elem)
}

/// The length of the array `place` is, if it is one of a known length.
fn array_len(ctx: &Ctx<'_, '_>, place: PlaceId) -> Option<u64> {
    let ty = ctx.places.borrow().ty(place).ty;
    match ty.kind() {
        TyKind::Array(_, len) => try_const_usize(ctx.db, len).map(|it| it as u64),
        _ => None,
    }
}

/// The place `up` elements above `place`.
fn ancestor(ctx: &Ctx<'_, '_>, mut place: PlaceId, up: usize) -> PlaceId {
    let places = ctx.places.borrow();
    for _ in 0..up {
        match places.parent(place) {
            Some(parent) => place = parent,
            None => break,
        }
    }
    place
}

impl<'f> Tree<'f> {
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

    fn gather_move(
        &mut self,
        ctx: &Ctx<'_, '_>,
        decoded: &Decoded<'_>,
        moved: Moved,
        location: Location,
    ) {
        let place = moved.place;
        let ty = ctx.places.borrow().ty(place).ty;
        let local = decoded.local(place);
        let projection = decoded.projection(place);
        let moves = match moved.kind {
            MoveKind::Operand => {
                !ctx.is_copy(ty) && !ctx.moves_out_of_guard(local, projection, moved.span)
            }
            MoveKind::Cast => !ctx.is_copy(ty),
            MoveKind::Always => true,
        };
        if !moves {
            return;
        }
        let base = ctx.places.borrow().parent(place);
        // A move of a subslice of an array moves each element of it.
        if let Some(ProjectionElem::Subslice { from, to }) = projection.last()
            && let Some(base) = base
            && let Lookup::Exact(array) = self.create(ctx, base)
            && let Some(len) = array_len(ctx, base)
        {
            for offset in *from..len.saturating_sub(*to) {
                let path = self.child(
                    array as u32,
                    local,
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
        match self.create(ctx, place) {
            Lookup::Exact(path) => self.moves.push(MoveOut { path, location }),
            // A move out of a field of a union moves the union, as long as nothing else on the
            // way makes it an error.
            Lookup::Parent(path) => {
                let depth = self.depth[path] as usize;
                let base = ancestor(ctx, place, projection.len() - depth);
                let base_ty = ctx.places.borrow().ty(base).ty;
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

    fn gather_init(
        &mut self,
        ctx: &Ctx<'_, '_>,
        decoded: &Decoded<'_>,
        written: Written,
        location: Location,
    ) {
        let kind = match written.kind {
            WriteKind::Call => InitKind::NonPanicPathOnly,
            WriteKind::Assign | WriteKind::Asm | WriteKind::DropAndReplace | WriteKind::Yield => {
                InitKind::Deep
            }
        };
        let place = written.place;
        let path = match self.create(ctx, place) {
            Lookup::Exact(path) => path,
            // Assigning a field of a union initializes the union.
            Lookup::Parent(path) if self.is_union_field(ctx, decoded, place, path) => path,
            Lookup::Parent(_) => return,
        };
        self.inits.push(Init { path, location: Some(location), kind });
    }

    fn is_union_field(
        &self,
        ctx: &Ctx<'_, '_>,
        decoded: &Decoded<'_>,
        place: PlaceId,
        parent: Mpi,
    ) -> bool {
        let projection = decoded.projection(place);
        let depth = self.depth[parent] as usize;
        if projection.len() != depth + 1 || !matches!(projection[depth], ProjectionElem::Field(_)) {
            return false;
        }
        let places = ctx.places.borrow();
        let Some(base) = places.parent(place) else { return false };
        matches!(places.ty(base).ty.kind(), TyKind::Adt(def, _) if def.is_union())
    }

    /// The path of `place`, made if it can have one: its parent's first, then the step down.
    fn create(&mut self, ctx: &Ctx<'_, '_>, place: PlaceId) -> Lookup {
        if let Some(&known) = self.lookups.get(place as usize)
            && known != NONE
        {
            return Lookup::unpack(known);
        }
        let (parent, local) = {
            let places = ctx.places.borrow();
            (places.parent(place), places.local(place))
        };
        let found = match parent {
            None => Lookup::Exact(place as Mpi),
            Some(parent) => match self.create(ctx, parent) {
                Lookup::Parent(path) => Lookup::Parent(path),
                Lookup::Exact(path) => match step_of(ctx, parent, place) {
                    Some(step) => Lookup::Exact(self.child(path as u32, local, step) as Mpi),
                    None => Lookup::Parent(path),
                },
            },
        };
        if self.lookups.len() <= place as usize {
            self.lookups.resize(place as usize + 1, NONE);
        }
        self.lookups[place as usize] = found.pack();
        found
    }

    /// The paths renumbered in preorder, without a stack: down to the first child, else on
    /// to the next sibling, else back up.
    fn freeze(self, frame: &'f Frame, cfg: &Cfg<'_>, decoded: &Decoded<'f>) -> MoveData<'f> {
        let count = self.local.len();
        let mut order: FVec<'f, u32> = Vec::with_capacity_in(count, frame);
        let mut renumbered = filled(frame, count, NONE);
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
        let mut local = Vec::with_capacity_in(count, frame);
        let mut parent = Vec::with_capacity_in(count, frame);
        let mut end = mem::collect(frame, 1..=count as u32);
        let mut projections = Csr::with_capacity(frame, count, depths);
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
        let mut steps = mem::map(frame);
        steps.reserve(count - self.roots);
        for old in self.roots..count {
            let up = renumbered[self.parent[old] as usize];
            steps.insert((up, self.step[old - self.roots]), renumbered[old]);
        }
        let mut lookups = self.lookups;
        for it in lookups.iter_mut().filter(|it| **it != NONE) {
            let lookup = match Lookup::unpack(*it) {
                Lookup::Exact(path) => Lookup::Exact(renumbered[path] as Mpi),
                Lookup::Parent(path) => Lookup::Parent(renumbered[path] as Mpi),
            };
            *it = lookup.pack();
        }
        let mut moves = self.moves;
        for it in moves.iter_mut() {
            it.path = renumbered[it.path] as Mpi;
        }
        let mut inits = self.inits;
        for it in inits.iter_mut() {
            it.path = renumbered[it.path] as Mpi;
        }
        let mut facts = Facts::with_capacity(frame, moves.len() * 2 + inits.len());
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
        let renumbered: &'f [u32] = renumbered.leak();
        MoveData {
            local,
            parent,
            end,
            projections,
            locals: &renumbered[..self.roots],
            steps,
            lookups: RefCell::new(lookups),
            moves,
            inits,
            facts,
            dead: decoded.dead,
            returns: decoded.returns,
            terminated: decoded.terminated,
        }
    }
}

impl<'db> Ctx<'_, 'db> {
    /// The step of a move path that `elem` of a place of type `ty` is, if it has one.
    pub(super) fn trackable(&self, ty: PlaceTy<'db>, elem: &PlaceElem) -> Option<PlaceElem> {
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
pub(super) struct InitFlow<'f> {
    /// A row of move paths per block.
    pub(super) uninit_entry: Rows<'f>,
    /// A row of locals per block.
    pub(super) ever_entry: Rows<'f>,
}

impl<'f> InitFlow<'f> {
    pub(super) fn compute(
        frame: &'f Frame,
        body: &MirBody<'_>,
        cfg: &Cfg<'_>,
        moves: &MoveData<'_>,
    ) -> Self {
        let blocks = body.basic_blocks.len();
        let paths = moves.len();
        let locals = body.locals.len();
        let mut uninit = Effects::new(frame, blocks, paths);
        let mut ever = Effects::new(frame, blocks, locals);
        // A call's return: what its destination being initialized does on that edge only, a
        // row per reachable call rather than per block.
        let returns = |block: BasicBlockId| moves.returns[index(block)] != NONE;
        let calls = cfg.order.iter().filter(|&&block| returns(block)).count();
        let mut returned_uninit = Effects::new(frame, calls, paths);
        let mut returned_ever = Effects::new(frame, calls, locals);
        // Per block, its call's row.
        let call_of: &'f mut [u32] = frame.alloc_slice_fill_with(blocks, |_| NONE);
        let mut call = 0;
        for &block in cfg.order.iter() {
            let (start, end) = (cfg.block_start(block), cfg.block_end(body, block));
            {
                let (mut uninit, mut ever) = (uninit.row(index(block)), ever.row(index(block)));
                for point in start..=end {
                    apply_at(moves, point, &mut uninit, &mut ever);
                }
            }
            if returns(block) {
                call_of[index(block)] = call as u32;
                let (mut uninit, mut ever) = (returned_uninit.row(call), returned_ever.row(call));
                call_returned(moves, end, &mut uninit, &mut ever);
                call += 1;
            }
        }
        let call_of: &[u32] = call_of;
        let mut uninit_start = filled_bits(frame, paths);
        let mut ever_start = bits(frame, locals);
        for init in moves.inits.iter().filter(|it| it.location.is_none()) {
            uninit_start.kill_range(moves.subtree(init.path));
            ever_start.insert(raw(moves.local(init.path)));
        }
        let uninit_entry = forward(
            frame,
            cfg,
            &uninit,
            body.start_block,
            uninit_start.words(),
            edges(moves, call_of, &returned_uninit),
        );
        let ever_entry = forward(
            frame,
            cfg,
            &ever,
            body.start_block,
            ever_start.words(),
            edges(moves, call_of, &returned_ever),
        );
        InitFlow { uninit_entry, ever_entry }
    }
}

/// What crosses each edge: a call's return initializes its destination on the edge to where
/// it returns (`returned`'s row `call_of[block]`), and a block with no terminator yet leads
/// nowhere.
fn edges<'e>(
    moves: &'e MoveData<'_>,
    call_of: &'e [u32],
    returned: &'e Effects<'_>,
) -> impl FnMut(BasicBlockId, BasicBlockId) -> Edge<'e> {
    move |from, to| {
        let at = index(from);
        if !moves.terminated[at] {
            Edge::Nothing
        } else if call_of[at] != NONE && moves.returns[at] == index(to) as u32 {
            returned.edge(call_of[at] as usize)
        } else {
            Edge::Same
        }
    }
}

/// The effect of the statement or terminator at `location` on what may be uninitialized and
/// what has ever been initialized, the return of a call aside: on the sets as the walk goes,
/// or composed into a block's transfer function.
pub(super) fn apply(
    _body: &MirBody<'_>,
    moves: &MoveData<'_>,
    cfg: &Cfg<'_>,
    location: Location,
    uninit: &mut impl Transfer,
    ever: &mut impl Transfer,
) {
    apply_at(moves, cfg.point(location), uninit, ever);
}

fn apply_at(
    moves: &MoveData<'_>,
    point: usize,
    uninit: &mut impl Transfer,
    ever: &mut impl Transfer,
) {
    for &index in moves.moves_at(point) {
        uninit.gen_range(moves.subtree(moves.moves[index as usize].path));
    }
    for &index in moves.inits_at(point) {
        let init = moves.inits[index as usize];
        if init.kind == InitKind::Deep {
            uninit.kill_range(moves.subtree(init.path));
            if moves.parent(init.path).is_none() {
                ever.gen_(raw(moves.local(init.path)));
            }
        }
    }
    let dead = moves.dead[point];
    if dead != NONE {
        ever.kill(dead as usize);
    }
}

/// What the call at `point` returning does: its destination is initialized.
fn call_returned(
    moves: &MoveData<'_>,
    point: usize,
    uninit: &mut impl Transfer,
    ever: &mut impl Transfer,
) {
    for &index in moves.inits_at(point) {
        let init = moves.inits[index as usize];
        if init.kind == InitKind::NonPanicPathOnly {
            uninit.kill_range(moves.subtree(init.path));
            if moves.parent(init.path).is_none() {
                ever.gen_(raw(moves.local(init.path)));
            }
        }
    }
}
