//! The parts of locals that a body moves out of or assigns on their own, and where each may hold
//! a value: rustc's move paths with its `MaybeInitializedPlaces` and
//! `MaybeUninitializedPlaces`, in one forward analysis.
//!
//! A move path is a local, or a field, variant field, element of an array of known length, or
//! the contents of a `Box` of another path. Only parts that are moved out of or assigned
//! somewhere in the body get one; the rest of a value goes with the path above it. A part
//! behind any other pointer, at an index that is only known when the body runs, or of a value
//! whose type implements `Drop` has none, since it cannot be moved out of. A field of a union
//! is the union.
//!
//! Everything here lives in the pass's frame: the paths, their subtrees, the effects of each
//! location, the states. A warm run allocates nothing on the global allocator.

use std::ops::Range;

use blaze_mem::Frame;
use rustc_type_ir::inherent::IntoKind;

use crate::{
    db::HirDatabase,
    consteval::try_const_usize,
    mir::{
        BasicBlockId, BorrowKind, FieldIndex, LocalId, MirBody, OperandKind, Place, PlaceElem,
        PlaceTy, Projection, ProjectionElem, Rvalue, StatementKind, StoredPlace, SwitchTargets,
        TerminatorKind,
        framed::{Bits, bits_in, block_index, local_index},
    },
    next_solver::TyKind,
};

use super::Ctx;

pub(super) type PathId = usize;

/// The blocks control goes to from `kind` when nothing unwinds, in the order of
/// [`TerminatorKind::successors`], without its `SmallVec`, which spills for a switch of more
/// than two targets.
pub(super) fn successors(kind: &TerminatorKind) -> impl Iterator<Item = BasicBlockId> + '_ {
    let (first, second, rest): (Option<BasicBlockId>, Option<BasicBlockId>, &[BasicBlockId]) =
        match kind {
            TerminatorKind::Goto { target }
            | TerminatorKind::Drop { target, .. }
            | TerminatorKind::DropAndReplace { target, .. }
            | TerminatorKind::Assert { target, .. }
            | TerminatorKind::FalseEdge { real_target: target, .. }
            | TerminatorKind::FalseUnwind { real_target: target, .. } => (Some(*target), None, &[]),
            TerminatorKind::SwitchInt { targets, .. } => (None, None, targets.all_targets()),
            TerminatorKind::Call { target, .. } => (*target, None, &[]),
            TerminatorKind::Yield { resume, drop, .. } => (Some(*resume), *drop, &[]),
            TerminatorKind::UnwindResume
            | TerminatorKind::Abort
            | TerminatorKind::Return
            | TerminatorKind::Unreachable
            | TerminatorKind::TailCall { .. }
            | TerminatorKind::CoroutineDrop => (None, None, &[]),
        };
    first.into_iter().chain(second).chain(rest.iter().copied())
}

/// How a move path is reached from the one above it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(super) enum Step {
    Field(FieldIndex),
    Variant(hir_def::VariantId),
    BoxContents,
    /// An element of an array whose length is known, counted from the front.
    Element(u64),
}

pub(super) struct MovePath<'f, 'db> {
    pub(super) place: StoredPlace,
    pub(super) ty: PlaceTy<'db>,
    pub(super) children: Vec<(Step, PathId), &'f Frame>,
}

/// What a place is in terms of move paths.
pub(super) enum Lookup {
    /// The place is this path.
    Exact(PathId),
    /// The place is a part of a path that has none of its own, as what a reference points
    /// to, or of a local that has no path at all.
    Untracked,
}

pub(super) struct MoveData<'f, 'db> {
    pub(super) paths: Vec<MovePath<'f, 'db>, &'f Frame>,
    /// By local index: its path.
    locals: &'f mut [Option<PathId>],
    frame: &'f Frame,
    /// Filled once the paths are all made, by `finish`: each path's subtree, as
    /// `subtree_all[subtree_start[p]..subtree_start[p + 1]]`.
    subtree_start: Vec<u32, &'f Frame>,
    subtree_all: Vec<PathId, &'f Frame>,
}

/// Whether the projection `elem` of a place of type `ty` gets a move path of its own.
enum Tracked {
    Step(Step),
    /// A field of a union: the union stands for it.
    Union,
    /// Moves cannot reach in here.
    No,
}

impl<'f, 'db> MoveData<'f, 'db> {
    /// The move paths of `body`: one for each local whose type has drop glue, and one for each
    /// part of those that the blocks in `live` move out of, assign or drop.
    pub(super) fn gather(
        ctx: &Ctx<'_, 'db>,
        body: &MirBody<'db>,
        live: &Bits<'_>,
        frame: &'f Frame,
    ) -> Self {
        let mut data = MoveData {
            paths: Vec::new_in(frame),
            locals: frame.alloc_slice_fill_with(body.locals.len(), |_| None),
            frame,
            subtree_start: Vec::new_in(frame),
            subtree_all: Vec::new_in(frame),
        };
        for (local, decl) in body.locals.iter() {
            let ty = decl.ty.as_ref();
            if ctx.needs_drop(ty) {
                let id = data.paths.len();
                data.paths.push(MovePath {
                    place: Place::from(local).store(),
                    ty: PlaceTy::from_ty(ty),
                    children: Vec::new_in(frame),
                });
                data.locals[local_index(local)] = Some(id);
            }
        }
        for (id, block) in body.basic_blocks.iter() {
            if !live.contains(block_index(id)) {
                continue;
            }
            for statement in &block.statements {
                if let StatementKind::Assign(place, _) = &statement.kind {
                    data.create(ctx, place);
                }
                statement.kind.for_each_operand(&mut |operand| {
                    if let OperandKind::Move(place) = &operand.kind {
                        data.create(ctx, place);
                    }
                });
            }
            let Some(terminator) = &block.terminator else { continue };
            terminator.kind.for_each_operand(&mut |operand| {
                if let OperandKind::Move(place) = &operand.kind {
                    data.create(ctx, place);
                }
            });
            match &terminator.kind {
                TerminatorKind::Call { destination: place, .. }
                | TerminatorKind::Drop { place, .. }
                | TerminatorKind::Yield { resume_arg: place, .. } => {
                    data.create(ctx, place);
                }
                _ => (),
            }
        }
        data.finish();
        data
    }

    /// Fills the subtree table, once every path is made: `child` is only called while
    /// gathering, so the table never goes stale.
    fn finish(&mut self) {
        let paths = &self.paths;
        fill_subtrees(
            paths.len(),
            |path| paths[path].children.as_slice(),
            &mut self.subtree_start,
            &mut self.subtree_all,
        );
    }

    pub(super) fn for_local(&self, local: LocalId) -> Option<PathId> {
        self.locals[local_index(local)]
    }

    /// Makes the paths down to `place`, as far as they can go.
    fn create(&mut self, ctx: &Ctx<'_, 'db>, place: &StoredPlace) {
        let Some(root) = self.locals[local_index(place.local)] else { return };
        let mut current = root;
        for elem in place.projection.as_slice() {
            match self.tracked(ctx, current, elem) {
                Tracked::Step(step) => current = self.child(ctx, current, step),
                // Each element of a subslice of an array is moved on its own.
                Tracked::No if matches!(elem, ProjectionElem::Subslice { .. }) => {
                    for index in self.subslice(ctx, current, elem) {
                        self.child(ctx, current, Step::Element(index));
                    }
                    return;
                }
                Tracked::Union | Tracked::No => return,
            }
        }
    }

    /// The indices of the elements that a subslice projection of `parent` covers, if `parent`
    /// is an array of known length; empty otherwise.
    fn subslice(&self, ctx: &Ctx<'_, 'db>, parent: PathId, elem: &PlaceElem) -> Range<u64> {
        let &ProjectionElem::Subslice { from, to } = elem else { return 0..0 };
        match self.array_len(ctx, parent) {
            Some(len) if from + to <= len => from..len - to,
            _ => 0..0,
        }
    }

    fn array_len(&self, ctx: &Ctx<'_, 'db>, path: PathId) -> Option<u64> {
        let ty = &self.paths[path].ty;
        if ty.variant_id.is_some() {
            return None;
        }
        match ctx.normalize(ty.ty).kind() {
            TyKind::Array(_, len) => try_const_usize(ctx.db, len).map(|it| it as u64),
            _ => None,
        }
    }

    fn tracked(&self, ctx: &Ctx<'_, 'db>, path: PathId, elem: &PlaceElem) -> Tracked {
        let place_ty = &self.paths[path].ty;
        if let Some(_variant) = place_ty.variant_id {
            return match elem {
                ProjectionElem::Field(field) => Tracked::Step(Step::Field(*field)),
                _ => Tracked::No,
            };
        }
        let ty = ctx.normalize(place_ty.ty);
        match (elem, ty.kind()) {
            (ProjectionElem::Deref, TyKind::Adt(adt, _)) if adt.is_box() => {
                Tracked::Step(Step::BoxContents)
            }
            (ProjectionElem::Field(_), TyKind::Adt(adt, _)) if adt.is_union() => Tracked::Union,
            (ProjectionElem::Field(field), TyKind::Adt(adt, _))
                if !adt.is_box() && !ctx.has_destructor(adt.def_id()) =>
            {
                Tracked::Step(Step::Field(*field))
            }
            (ProjectionElem::Field(field), TyKind::Tuple(_) | TyKind::Closure(..)) => {
                Tracked::Step(Step::Field(*field))
            }
            (ProjectionElem::Downcast(variant), TyKind::Adt(adt, _))
                if adt.is_enum() && !ctx.has_destructor(adt.def_id()) =>
            {
                Tracked::Step(Step::Variant(*variant))
            }
            (&ProjectionElem::ConstantIndex { offset, from_end, .. }, TyKind::Array(_, len)) => {
                let Some(len) = try_const_usize(ctx.db, len).map(|it| it as u64) else {
                    return Tracked::No;
                };
                let index = if from_end { len.checked_sub(offset) } else { Some(offset) };
                match index {
                    Some(index) if index < len => Tracked::Step(Step::Element(index)),
                    _ => Tracked::No,
                }
            }
            _ => Tracked::No,
        }
    }

    /// The path `step` below `parent`, made if there is none yet.
    pub(super) fn child(&mut self, ctx: &Ctx<'_, 'db>, parent: PathId, step: Step) -> PathId {
        if let Some(&(_, id)) = self.paths[parent].children.iter().find(|(it, _)| *it == step) {
            return id;
        }
        let parent_place = self.paths[parent].place.as_ref();
        let elem = match step {
            Step::Field(field) => ProjectionElem::Field(field),
            Step::Variant(variant) => ProjectionElem::Downcast(variant),
            Step::BoxContents => ProjectionElem::Deref,
            Step::Element(index) => ProjectionElem::ConstantIndex {
                offset: index,
                min_length: index + 1,
                from_end: false,
            },
        };
        let ty = self.paths[parent].ty.projection_ty(&ctx.infcx, &elem, ctx.env);
        let place = Place {
            local: parent_place.local,
            projection: Projection::new_from_iter(
                parent_place.projection.as_slice().iter().copied().chain([elem]),
            ),
        };
        let id = self.paths.len();
        self.paths.push(MovePath { place: place.store(), ty, children: Vec::new_in(self.frame) });
        self.paths[parent].children.push((step, id));
        id
    }

    /// The path `step` below `parent`, if there is one.
    pub(super) fn find_child(&self, parent: PathId, step: Step) -> Option<PathId> {
        self.paths[parent].children.iter().find(|(it, _)| *it == step).map(|&(_, id)| id)
    }

    /// Calls `f` with each path that `place` is: one, or the elements of a subslice of an
    /// array. A part of a union is the union. None for a place that has no path, and for a part
    /// of one that has no path of its own, as for a move out of a place behind a reference.
    pub(super) fn paths_of(
        &self,
        ctx: &Ctx<'_, 'db>,
        place: &StoredPlace,
        mut f: impl FnMut(PathId),
    ) {
        let Some(root) = self.locals[local_index(place.local)] else { return };
        let mut current = root;
        for elem in place.projection.as_slice() {
            match self.tracked(ctx, current, elem) {
                Tracked::Step(step) => match self.find_child(current, step) {
                    Some(child) => current = child,
                    None => return,
                },
                Tracked::Union => return f(current),
                Tracked::No if matches!(elem, ProjectionElem::Subslice { .. }) => {
                    for index in self.subslice(ctx, current, elem) {
                        if let Some(it) = self.find_child(current, Step::Element(index)) {
                            f(it);
                        }
                    }
                    return;
                }
                Tracked::No => return,
            }
        }
        f(current)
    }

    /// What `place` is in terms of paths, for a drop of it.
    pub(super) fn lookup(&self, ctx: &Ctx<'_, 'db>, place: &StoredPlace) -> Lookup {
        let Some(root) = self.locals[local_index(place.local)] else { return Lookup::Untracked };
        let mut current = root;
        for elem in place.projection.as_slice() {
            match self.tracked(ctx, current, elem) {
                Tracked::Step(step) => match self.find_child(current, step) {
                    Some(child) => current = child,
                    None => return Lookup::Untracked,
                },
                Tracked::Union | Tracked::No => return Lookup::Untracked,
            }
        }
        Lookup::Exact(current)
    }

    /// `path` and every path below it, breadth first.
    pub(super) fn subtree(&self, path: PathId) -> &[PathId] {
        debug_assert!(!self.subtree_start.is_empty(), "the subtrees are filled by `gather`");
        let start = self.subtree_start[path] as usize;
        let end = self.subtree_start[path + 1] as usize;
        &self.subtree_all[start..end]
    }

    /// Whether no part of `place` may hold a value in `state`: rustc's `is_unwind_dead`.
    pub(super) fn holds_nothing(
        &self,
        ctx: &Ctx<'_, 'db>,
        place: &StoredPlace,
        state: &InitState<'_>,
    ) -> bool {
        match self.lookup(ctx, place) {
            Lookup::Exact(path) => {
                self.subtree(path).iter().all(|&it| !state.maybe_init.contains(it))
            }
            Lookup::Untracked => false,
        }
    }

    /// Fills `out` with the enum that the `SwitchInt` ending `block` switches on the variant
    /// of, when the block's last statement reads its discriminant: rustc's
    /// `MaybePlacesSwitchIntData`. Whether it does; `out` means nothing when not.
    pub(super) fn switch_on_variant(
        &self,
        ctx: &Ctx<'_, 'db>,
        body: &MirBody<'db>,
        block: BasicBlockId,
        out: &mut VariantSwitch<'_>,
    ) -> bool {
        let block = &body.basic_blocks[block];
        let Some(TerminatorKind::SwitchInt { discr, .. }) =
            block.terminator.as_ref().map(|it| &it.kind)
        else {
            return false;
        };
        let (OperandKind::Copy(discr) | OperandKind::Move(discr)) = &discr.kind else {
            return false;
        };
        let Some(StatementKind::Assign(read, Rvalue::Discriminant(place))) =
            block.statements.last().map(|it| &it.kind)
        else {
            return false;
        };
        if read != discr {
            return false;
        }
        let ty = ctx.normalize(place.as_ref().ty(body, &ctx.infcx, ctx.env).ty);
        let TyKind::Adt(adt, _) = ty.kind() else { return false };
        let hir_def::AdtId::EnumId(id) = adt.def_id() else { return false };
        let Lookup::Exact(path) = self.lookup(ctx, place) else { return false };
        let discr_ty = ctx.normalize(discr.as_ref().ty(body, &ctx.infcx, ctx.env).ty);
        if !matches!(discr_ty.kind(), TyKind::Int(_) | TyKind::Uint(_)) {
            return false;
        }
        let mask = match discr_ty.primitive_size(ctx.infcx.interner).bytes() {
            bytes @ ..16 => (1u128 << (bytes * 8)) - 1,
            _ => u128::MAX,
        };
        out.variants.clear();
        out.children.clear();
        for &(variant, _) in id.enum_variants(ctx.db).variants.values() {
            let Ok(value) = ctx.db.const_eval_discriminant(variant) else { return false };
            out.variants.push((hir_def::VariantId::from(variant), value as u128 & mask));
        }
        out.children.extend(self.paths[path].children.iter().filter_map(|&(step, child)| {
            match step {
                Step::Variant(variant) => Some((variant, child)),
                _ => None,
            }
        }));
        true
    }
}

/// Writes the subtree of each of `count` paths, breadth first, into `all`, the subtree of `p`
/// at `all[start[p]..start[p + 1]]`.
fn fill_subtrees<'c>(
    count: usize,
    children: impl Fn(PathId) -> &'c [(Step, PathId)],
    start: &mut Vec<u32, &Frame>,
    all: &mut Vec<PathId, &Frame>,
) {
    start.clear();
    all.clear();
    start.reserve_exact(count + 1);
    all.reserve(count);
    for path in 0..count {
        let first = all.len();
        start.push(first as u32);
        all.push(path);
        let mut at = first;
        while at < all.len() {
            let parent = all[at];
            all.extend(children(parent).iter().map(|&(_, id)| id));
            at += 1;
        }
    }
    start.push(all.len() as u32);
}

/// A `SwitchInt` on the variant of an enum, with the paths of the enum's variants. Filled by
/// [`MoveData::switch_on_variant`], and reused from one switch to the next.
pub(super) struct VariantSwitch<'f> {
    variants: Vec<(hir_def::VariantId, u128), &'f Frame>,
    children: Vec<(hir_def::VariantId, PathId), &'f Frame>,
}

impl<'f> VariantSwitch<'f> {
    pub(super) fn new_in(frame: &'f Frame) -> Self {
        VariantSwitch { variants: Vec::new_in(frame), children: Vec::new_in(frame) }
    }

    /// Fills `out` with each edge of the switch and the paths of the variants that are not
    /// the one the enum holds along it, which hold nothing there (`on_all_inactive_variants`):
    /// every variant but a value's own, and on the `otherwise` edge those of the values the
    /// switch names.
    pub(super) fn edges(&self, targets: &SwitchTargets, out: &mut Edges<'_>) {
        let variant_of =
            |value: u128| self.variants.iter().find(|&&(_, it)| it == value).map(|&(it, _)| it);
        out.clear();
        for (value, to) in targets.iter() {
            let Some(active) = variant_of(value) else {
                out.clear();
                for &to in targets.all_targets() {
                    out.push(to, std::iter::empty());
                }
                return;
            };
            out.push(to, self.children.iter().filter(|&&(it, _)| it != active).map(|&(_, p)| p));
        }
        // Every value has a variant by now, or the edges went back above.
        let named = |variant| targets.iter().any(|(value, _)| variant_of(value) == Some(variant));
        out.push(
            targets.otherwise(),
            self.children.iter().filter(|&&(it, _)| named(it)).map(|&(_, p)| p),
        );
    }
}

/// The edges of a switch, each with the paths that hold nothing along it.
pub(super) struct Edges<'f> {
    /// (target, end of its paths in `paths`)
    to: Vec<(BasicBlockId, u32), &'f Frame>,
    paths: Vec<PathId, &'f Frame>,
}

impl<'f> Edges<'f> {
    pub(super) fn new_in(frame: &'f Frame) -> Self {
        Edges { to: Vec::new_in(frame), paths: Vec::new_in(frame) }
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = (BasicBlockId, &[PathId])> {
        let mut start = 0;
        self.to.iter().map(move |&(to, end)| {
            let paths = &self.paths[start..end as usize];
            start = end as usize;
            (to, paths)
        })
    }

    fn push(&mut self, to: BasicBlockId, paths: impl Iterator<Item = PathId>) {
        self.paths.extend(paths);
        self.to.push((to, self.paths.len() as u32));
    }

    fn clear(&mut self) {
        self.to.clear();
        self.paths.clear();
    }
}

/// Which paths may hold a value, and which may not, at one point of the body.
#[derive(PartialEq, Eq, Debug)]
pub(super) struct InitState<'f> {
    pub(super) maybe_init: Bits<'f>,
    pub(super) maybe_uninit: Bits<'f>,
}

impl<'f> InitState<'f> {
    pub(super) fn bottom(len: usize, frame: &'f Frame) -> Self {
        InitState { maybe_init: bits_in(len, frame), maybe_uninit: bits_in(len, frame) }
    }

    /// `other`'s members in place of these, without allocating: both are of one `len`.
    pub(super) fn copy_from(&mut self, other: &InitState<'_>) {
        self.maybe_init.copy_from(&other.maybe_init);
        self.maybe_uninit.copy_from(&other.maybe_uninit);
    }

    /// A copy in `frame`.
    pub(super) fn copy_in<'g>(&self, frame: &'g Frame) -> InitState<'g> {
        let mut copy = InitState::bottom(self.maybe_init.capacity(), frame);
        copy.copy_from(self);
        copy
    }

    fn join(&mut self, other: &InitState<'_>) -> bool {
        let init = self.maybe_init.union_with(&other.maybe_init);
        let uninit = self.maybe_uninit.union_with(&other.maybe_uninit);
        init || uninit
    }

    pub(super) fn set(&mut self, paths: &[PathId], init: bool) {
        for &path in paths {
            if init {
                self.maybe_init.insert(path);
                self.maybe_uninit.remove(path);
            } else {
                self.maybe_init.remove(path);
                self.maybe_uninit.insert(path);
            }
        }
    }

    pub(super) fn apply(&mut self, paths: &[PathId], change: Change) {
        match change {
            Change::Init => self.set(paths, true),
            Change::Uninit => self.set(paths, false),
            Change::Borrowed => {
                for &path in paths {
                    self.maybe_init.insert(path);
                }
            }
        }
    }

    /// (maybe init, maybe uninit) of `path` itself.
    pub(super) fn of(&self, path: PathId) -> (bool, bool) {
        (self.maybe_init.contains(path), self.maybe_uninit.contains(path))
    }
}

/// Something a statement or terminator does to the paths, with every path below the one it
/// names.
#[derive(Clone, Copy, Debug)]
pub(super) struct Effect {
    pub(super) path: PathId,
    pub(super) change: Change,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Change {
    /// An assignment: the paths hold a value.
    Init,
    /// A move out, a drop, `StorageDead`: they hold none.
    Uninit,
    /// A mutable borrow (`&mut`, `&raw mut`), through which they may be assigned: they may hold a
    /// value, and may still hold none, as rustc's `MaybeInitializedPlaces` has it (#90752: an enum
    /// a `match` found `None` and then `*(&mut x) = Some(..)`). It writes no drop flag. rustc
    /// counts `&raw const` too, which MIR lowering here makes a shared borrow.
    Borrowed,
}

/// Pushes onto `effects` the effects of the statement `at` of `block`, or of its terminator
/// when `at` is the number of statements, apart from what a terminator does on one of its
/// edges only. In the order they happen: moves out of the operands, then the write.
pub(super) fn effects_at<'db>(
    ctx: &Ctx<'_, 'db>,
    data: &MoveData<'_, 'db>,
    body: &MirBody<'db>,
    block: BasicBlockId,
    at: usize,
    effects: &mut Vec<Effect, &Frame>,
) {
    let moved = |operand: &crate::mir::Operand, effects: &mut Vec<Effect, &Frame>| {
        if let OperandKind::Move(place) = &operand.kind {
            data.paths_of(ctx, place, |path| effects.push(Effect { path, change: Change::Uninit }));
        }
    };
    let data_block = &body.basic_blocks[block];
    if let Some(statement) = data_block.statements.get(at) {
        statement.kind.for_each_operand(&mut |operand| moved(operand, effects));
        match &statement.kind {
            StatementKind::Assign(place, rvalue) => {
                data.paths_of(ctx, &init_target(ctx, body, place), |path| {
                    effects.push(Effect { path, change: Change::Init })
                });
                if let Rvalue::Ref(BorrowKind::Mut { .. }, borrowed) = rvalue
                    && let Lookup::Exact(path) = data.lookup(ctx, borrowed)
                {
                    effects.push(Effect { path, change: Change::Borrowed });
                }
            }
            StatementKind::Deinit(place) => {
                data.paths_of(ctx, place, |path| {
                    effects.push(Effect { path, change: Change::Uninit })
                });
            }
            StatementKind::StorageDead(local) => {
                if let Some(path) = data.for_local(*local) {
                    effects.push(Effect { path, change: Change::Uninit });
                }
            }
            StatementKind::InlineAsm(asm) => {
                for operand in &asm.operands {
                    if let crate::mir::AsmOperand::Reg { output: Some(place), .. } = operand {
                        data.paths_of(ctx, place, |path| {
                            effects.push(Effect { path, change: Change::Init })
                        });
                    }
                }
            }
            StatementKind::FakeRead(_) | StatementKind::StorageLive(_) | StatementKind::Nop => (),
        }
        return;
    }
    let Some(terminator) = &data_block.terminator else { return };
    terminator.kind.for_each_operand(&mut |operand| moved(operand, effects));
    if let TerminatorKind::Drop { place, .. } = &terminator.kind {
        data.paths_of(ctx, place, |path| effects.push(Effect { path, change: Change::Uninit }));
    }
}

/// The place an assignment to `place` initializes: the union, for a field of one.
fn init_target<'db>(ctx: &Ctx<'_, 'db>, body: &MirBody<'db>, place: &StoredPlace) -> StoredPlace {
    let projection = place.projection.as_slice();
    if let Some((ProjectionElem::Field(_), base)) = projection.split_last() {
        let base_place = Place { local: place.local, projection: Projection::new_from_slice(base) };
        let base_ty = base_place.ty(body, &ctx.infcx, ctx.env);
        if base_ty.variant_id.is_none()
            && let TyKind::Adt(adt, _) = ctx.normalize(base_ty.ty).kind()
            && adt.is_union()
        {
            return base_place.store();
        }
    }
    place.clone() // clone: an interned place, a refcount
}

/// Calls `f` with what the edge from the terminator of a block to `to` initializes on its own:
/// the destination of a call when it returns, what a coroutine is resumed with.
pub(super) fn edge_effects<'db>(
    ctx: &Ctx<'_, 'db>,
    data: &MoveData<'_, 'db>,
    body: &MirBody<'db>,
    from: BasicBlockId,
    to: BasicBlockId,
    f: impl FnMut(PathId),
) {
    match body.basic_blocks[from].terminator.as_ref().map(|it| &it.kind) {
        Some(TerminatorKind::Call { destination, target: Some(target), .. }) if *target == to => {
            data.paths_of(ctx, destination, f)
        }
        Some(TerminatorKind::Yield { resume_arg, resume, .. }) if *resume == to => {
            data.paths_of(ctx, resume_arg, f)
        }
        _ => (),
    }
}

/// The effects of each statement and terminator of the blocks asked for, made the first time
/// a block is asked for: what `effects_at` says, kept for the analyses' and the rewrite's
/// later visits, so each location's effects are found once per pass.
pub(super) struct Effects<'f> {
    /// By block index: where the block's locations start in `starts`, `u32::MAX` until asked.
    block_start: &'f mut [u32],
    /// By location: where its effects start in `all`; each block's run ends with one more.
    starts: Vec<u32, &'f Frame>,
    all: Vec<Effect, &'f Frame>,
}

impl<'f> Effects<'f> {
    pub(super) fn new_in(blocks: usize, frame: &'f Frame) -> Self {
        Effects {
            block_start: frame.alloc_slice_fill_with(blocks, |_| u32::MAX),
            starts: Vec::new_in(frame),
            all: Vec::new_in(frame),
        }
    }

    /// The effects at statement `at` of `block`, its terminator when `at` is the number of
    /// statements. The body must be the one the table was made for, unchanged.
    pub(super) fn at<'db>(
        &mut self,
        ctx: &Ctx<'_, 'db>,
        data: &MoveData<'_, 'db>,
        body: &MirBody<'db>,
        block: BasicBlockId,
        at: usize,
    ) -> &[Effect] {
        let index = block_index(block);
        if self.block_start[index] == u32::MAX {
            self.block_start[index] = self.starts.len() as u32;
            for at in 0..=body.basic_blocks[block].statements.len() {
                self.starts.push(self.all.len() as u32);
                effects_at(ctx, data, body, block, at, &mut self.all);
            }
            self.starts.push(self.all.len() as u32);
        }
        let first = self.block_start[index] as usize + at;
        &self.all[self.starts[first] as usize..self.starts[first + 1] as usize]
    }
}

/// The blocks of the body control can reach from where it starts without going into a block
/// that rustc builds no way into (`unreached`, the blocks of [`MirBody::unreached`]), cleanup
/// blocks included.
pub(super) fn live_blocks<'f>(
    body: &MirBody<'_>,
    unreached: &Bits<'_>,
    frame: &'f Frame,
) -> Bits<'f> {
    let mut live = bits_in(body.basic_blocks.len(), frame);
    let mut stack = Vec::new_in(frame);
    stack.push(body.start_block);
    while let Some(id) = stack.pop() {
        if !live.insert(block_index(id)) {
            continue;
        }
        let block = &body.basic_blocks[id];
        let Some(terminator) = &block.terminator else { continue };
        let next = successors(&terminator.kind)
            .filter(|&it| !unreached.contains(block_index(it)))
            .chain(terminator.kind.unwind());
        for it in next {
            if !live.contains(block_index(it)) {
                stack.push(it);
            }
        }
    }
    live
}

/// Where each path may hold a value where each live block starts.
pub(super) struct InitAnalysis<'f> {
    pub(super) entry: Vec<Option<InitState<'f>>, &'f Frame>,
}

impl<'f> InitAnalysis<'f> {
    /// The analysis, which does not go into cleanup from the drops in `dead_unwinds` with what
    /// may be uninitialized there, as rustc's `MaybeUninitializedPlaces` skips their unwind
    /// edges (`skipping_unreachable_unwind`).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn run<'db>(
        ctx: &Ctx<'_, 'db>,
        data: &MoveData<'_, 'db>,
        body: &MirBody<'db>,
        live: &Bits<'_>,
        unreached: &Bits<'_>,
        dead_unwinds: &Bits<'_>,
        effects: &mut Effects<'_>,
        frame: &'f Frame,
    ) -> Self {
        let blocks = body.basic_blocks.len();
        let len = data.paths.len();
        let mut entry: Vec<Option<InitState<'f>>, &'f Frame> =
            Vec::with_capacity_in(blocks, frame);
        entry.resize_with(blocks, || None);
        let mut start = InitState::bottom(len, frame);
        start.maybe_uninit.insert_range(0..len);
        for param in body.entry_locals() {
            if let Some(path) = data.for_local(param) {
                start.set(data.subtree(path), true);
            }
        }
        entry[block_index(body.start_block)] = Some(start);
        let mut pending = Vec::new_in(frame);
        pending.push(body.start_block);
        let mut queued = bits_in(blocks, frame);
        queued.insert(block_index(body.start_block));
        // Reused from one block to the next: the state as the block goes, as it unwinds, and
        // along one edge.
        let mut state = InitState::bottom(len, frame);
        let mut unwound = InitState::bottom(len, frame);
        let mut edge = InitState::bottom(len, frame);
        let mut switch = VariantSwitch::new_in(frame);
        let mut edges = Edges::new_in(frame);
        while let Some(id) = pending.pop() {
            queued.remove(block_index(id));
            let block = &body.basic_blocks[id];
            let known = entry[block_index(id)].as_ref().expect("a block is queued with a state");
            state.copy_from(known);
            let mut flow = |to: BasicBlockId,
                            state: &InitState<'_>,
                            entry: &mut Vec<Option<InitState<'f>>, &'f Frame>,
                            pending: &mut Vec<BasicBlockId, &'f Frame>| {
                if !live.contains(block_index(to)) {
                    return;
                }
                let changed = match &mut entry[block_index(to)] {
                    Some(known) => known.join(state),
                    slot @ None => {
                        *slot = Some(state.copy_in(frame));
                        true
                    }
                };
                if changed && queued.insert(block_index(to)) {
                    pending.push(to);
                }
            };
            for at in 0..block.statements.len() {
                for effect in effects.at(ctx, data, body, id, at) {
                    state.apply(data.subtree(effect.path), effect.change);
                }
            }
            let Some(terminator) = &block.terminator else { continue };
            // rustc's `skipping_unreachable_unwind`: a drop of a place that holds nothing does
            // nothing, so nothing unwinds out of it. `MaybeInitializedPlaces` finds that as it
            // goes, `MaybeUninitializedPlaces` takes the drops the first found.
            let (skip_init, skip_uninit) = match &terminator.kind {
                TerminatorKind::Drop { place, unwind: Some(_), .. } => (
                    data.holds_nothing(ctx, place, &state),
                    dead_unwinds.contains(block_index(id)),
                ),
                _ => (false, false),
            };
            for effect in effects.at(ctx, data, body, id, block.statements.len()) {
                state.apply(data.subtree(effect.path), effect.change);
            }
            if let Some(cleanup) = terminator.kind.unwind() {
                unwound.copy_from(&state);
                if skip_init {
                    unwound.maybe_init.clear();
                }
                if skip_uninit {
                    unwound.maybe_uninit.clear();
                }
                flow(cleanup, &unwound, &mut entry, &mut pending);
            }
            if let TerminatorKind::SwitchInt { targets, .. } = &terminator.kind
                && data.switch_on_variant(ctx, body, id, &mut switch)
            {
                switch.edges(targets, &mut edges);
                for (to, inactive) in edges.iter() {
                    if unreached.contains(block_index(to)) {
                        continue;
                    }
                    edge.copy_from(&state);
                    for &path in inactive {
                        edge.set(data.subtree(path), false);
                    }
                    flow(to, &edge, &mut entry, &mut pending);
                }
                continue;
            }
            for to in successors(&terminator.kind) {
                if unreached.contains(block_index(to)) {
                    continue;
                }
                // The paths the edge initializes are disjoint from what `set` changes, so
                // applying them as they come is the same as collecting them first.
                let mut any = false;
                edge_effects(ctx, data, body, id, to, |path| {
                    if !any {
                        edge.copy_from(&state);
                        any = true;
                    }
                    edge.set(data.subtree(path), true);
                });
                flow(to, if any { &edge } else { &state }, &mut entry, &mut pending);
            }
        }
        InitAnalysis { entry }
    }

    /// rustc's `compute_dead_unwinds`: the drops that may unwind of a place that holds nothing
    /// where they are, as a set of block indices. Such a drop does nothing, so nothing unwinds
    /// out of it: the operand of an aggregate a scope drops after the aggregate took it, say.
    pub(super) fn dead_unwinds<'g, 'db>(
        &self,
        ctx: &Ctx<'_, 'db>,
        data: &MoveData<'_, 'db>,
        body: &MirBody<'db>,
        effects: &mut Effects<'_>,
        frame: &'g Frame,
    ) -> Bits<'g> {
        let mut dead = bits_in(body.basic_blocks.len(), frame);
        let mut state = InitState::bottom(data.paths.len(), frame);
        for (id, block) in body.basic_blocks.iter() {
            let Some(TerminatorKind::Drop { place, unwind: Some(_), .. }) =
                block.terminator.as_ref().map(|it| &it.kind)
            else {
                continue;
            };
            let Some(entry) = &self.entry[block_index(id)] else { continue };
            state.copy_from(entry);
            for at in 0..block.statements.len() {
                for effect in effects.at(ctx, data, body, id, at) {
                    state.apply(data.subtree(effect.path), effect.change);
                }
            }
            if data.holds_nothing(ctx, place, &state) {
                dead.insert(block_index(id));
            }
        }
        dead
    }
}

#[cfg(test)]
mod tests {
    use blaze_mem::{Tag, frame};
    use la_arena::{Idx, RawIdx};

    use super::*;

    fn block(at: u32) -> BasicBlockId {
        Idx::from_raw(RawIdx::from_u32(at))
    }

    /// The subtree of `path` as the walk over `children` before the table had it.
    fn walked(children: &[&[(Step, PathId)]], path: PathId) -> Vec<PathId> {
        let mut all = vec![path];
        let mut at = 0;
        while at < all.len() {
            all.extend(children[all[at]].iter().map(|&(_, id)| id));
            at += 1;
        }
        all
    }

    #[test]
    fn the_subtree_table_is_the_breadth_first_walk() {
        // 0 has 1 and 2; 1 has 3 and 4; 4 has 5; 2, 3 and 5 have none; 6 stands alone.
        let element = Step::Element;
        let children: [&[(Step, PathId)]; 7] = [
            &[(element(0), 1), (element(1), 2)],
            &[(element(0), 3), (element(1), 4)],
            &[],
            &[],
            &[(element(0), 5)],
            &[],
            &[],
        ];
        frame::task(Tag::Mir, |frame| {
            let mut start = Vec::new_in(frame);
            let mut all = Vec::new_in(frame);
            fill_subtrees(children.len(), |path| children[path], &mut start, &mut all);
            assert_eq!(start.len(), children.len() + 1);
            for path in 0..children.len() {
                let table = &all[start[path] as usize..start[path + 1] as usize];
                assert_eq!(table, walked(&children, path).as_slice(), "the subtree of {path}");
            }
            assert_eq!(&all[start[0] as usize..start[1] as usize], &[0, 1, 2, 3, 4, 5]);
        });
    }

    #[test]
    fn edges_give_back_each_target_with_its_own_paths() {
        frame::task(Tag::Mir, |frame| {
            let mut edges = Edges::new_in(frame);
            edges.push(block(3), [1, 2].into_iter());
            edges.push(block(4), std::iter::empty());
            edges.push(block(5), [7].into_iter());
            let seen: Vec<_> = edges.iter().map(|(to, paths)| (to, paths.to_vec())).collect();
            assert_eq!(seen, [(block(3), vec![1, 2]), (block(4), vec![]), (block(5), vec![7])]);
            edges.clear();
            assert_eq!(edges.iter().count(), 0);
            edges.push(block(1), [0].into_iter());
            let seen: Vec<_> = edges.iter().map(|(to, paths)| (to, paths.to_vec())).collect();
            assert_eq!(seen, [(block(1), vec![0])]);
        });
    }

    #[test]
    fn states_set_join_and_copy_in_place() {
        frame::task(Tag::Mir, |frame| {
            let len = 70;
            let mut state = InitState::bottom(len, frame);
            state.maybe_uninit.insert_range(0..len);
            state.set(&[0, 65], true);
            assert_eq!(state.of(0), (true, false));
            assert_eq!(state.of(65), (true, false));
            assert_eq!(state.of(1), (false, true));
            state.apply(&[1], Change::Borrowed);
            assert_eq!(state.of(1), (true, true));
            state.apply(&[0], Change::Uninit);
            assert_eq!(state.of(0), (false, true));

            let copy = state.copy_in(frame);
            assert_eq!(copy, state);
            let mut other = InitState::bottom(len, frame);
            other.copy_from(&state);
            assert_eq!(other, state);
            assert!(!other.join(&state), "a join with itself changes nothing");
            let mut more = InitState::bottom(len, frame);
            more.set(&[69], true);
            assert!(other.join(&more));
            assert_eq!(other.of(69), (true, true));

            other.maybe_init.clear();
            assert!(other.maybe_init.is_empty());
            assert!(!other.maybe_uninit.is_empty());
        });
    }
}
