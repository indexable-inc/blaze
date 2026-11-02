//! Drop elaboration, as rustc's `elaborate_drops` and `elaborate_drop`: every `Drop` of a
//! place that may or may not hold a value, or may hold part of one, becomes blocks that drop
//! exactly what it holds. What may hold a value where is decided from the move paths and the
//! maybe-init and maybe-uninit analyses; where that is only known when the body runs a drop
//! flag says it, a `bool` local that the body sets wherever the value is moved, assigned or
//! dropped.
//!
//! Where a drop is elaborated the place is in one of four states (rustc's `DropStyle`):
//! `Dead`, nothing to drop; `Static`, all of it to drop; `Conditional`, all or nothing,
//! which a flag says; `Open`, parts of it moved out, so each part is dropped on its own.
//! An open drop of a struct, tuple or closure drops the fields in order; of an enum it
//! switches on the variant; of a `Box` it drops the contents, then frees the box; of an array
//! it drops the elements that have a path of their own one by one and the runs between them
//! whole, from the back. If a drop of one part unwinds, the parts after it are dropped on the
//! way out, as rustc's drop ladders do.
//!
//! What the pass keeps while it runs is in its frame; what it adds to the body is grown once,
//! to its final size.

use std::ops::Range;

use blaze_mem::Frame;
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind, Ty as _};

use crate::{
    db::HirDatabase,
    consteval::try_const_usize,
    mir::{
        BasicBlock, BasicBlockId, BorrowKind, FieldIndex, Local, LocalId, MirBody, MirLowerError,
        MirSpan, MutBorrowKind, Operand, OperandKind, Place, PlaceElem, Projection, ProjectionElem,
        Rvalue, Statement, StatementKind, StoredPlace, SwitchTargets, Terminator, TerminatorKind,
        framed::{bits_in, block_index},
    },
    next_solver::{GenericArgs, Region, RegionExt, Ty, TyKind},
};

use super::{
    Ctx,
    move_paths::{
        self, Change, Effects, InitAnalysis, InitState, Lookup, MoveData, PathId, Step,
        edge_effects, successors,
    },
};

/// Where a drop that unwinds goes.
#[derive(Clone, Copy, Debug)]
enum Unwind {
    To(BasicBlockId),
    /// The drop is on the way out already: it cannot unwind again.
    InCleanup,
}

impl Unwind {
    fn is_cleanup(self) -> bool {
        matches!(self, Unwind::InCleanup)
    }

    fn action(self) -> Option<BasicBlockId> {
        match self {
            Unwind::To(target) => Some(target),
            Unwind::InCleanup => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Style {
    Dead,
    Static,
    Conditional,
    Open,
}

/// `'c` is the frame of the body's pipeline, which the context's caches live in; `'f` is this
/// pass's.
struct Elaborator<'a, 'c, 'f, 'db> {
    ctx: &'a Ctx<'c, 'db>,
    body: &'a mut MirBody<'db>,
    data: &'a MoveData<'f, 'db>,
    frame: &'f Frame,
    /// By path: its flag, once it has one.
    flags: &'f mut [Option<LocalId>],
    flag_count: usize,
    /// Where each path may hold a value at the drop being elaborated.
    state: InitState<'f>,
    span: MirSpan,
    /// A block that goes on unwinding, for a drop outside a cleanup block that names none.
    resume: Option<BasicBlockId>,
}

/// A drop in the body as lowering made it, before it is elaborated.
struct DropSite<'f> {
    block: BasicBlockId,
    place: StoredPlace,
    target: BasicBlockId,
    unwind: Option<BasicBlockId>,
    is_cleanup: bool,
    span: MirSpan,
    state: InitState<'f>,
}

/// A flag write once it is known which paths have one: to the flags of the subtree of `path`.
struct FlagWrite {
    path: PathId,
    value: bool,
}

/// What drop elaboration's analyses found, for its rewrite.
struct Analysed<'f, 'db> {
    data: MoveData<'f, 'db>,
    analysis: InitAnalysis<'f>,
    effects: Effects<'f>,
}

/// The analyses drop elaboration starts from, without its rewrite: what the warm budget test
/// holds to no allocation.
pub(super) fn analyse<'db>(ctx: &Ctx<'_, 'db>, body: &MirBody<'db>, frame: &Frame) {
    analysed(ctx, body, frame);
}

/// The live blocks, the move paths, both analyses and the drops that cannot unwind.
fn analysed<'f, 'db>(
    ctx: &Ctx<'_, 'db>,
    body: &MirBody<'db>,
    frame: &'f Frame,
) -> Analysed<'f, 'db> {
    let blocks = body.basic_blocks.len();
    let mut unreached = bits_in(blocks, frame);
    for &it in &body.unreached {
        unreached.insert(block_index(it));
    }
    let live = move_paths::live_blocks(body, &unreached, frame);
    let data = MoveData::gather(ctx, body, &live, frame);
    let mut effects = Effects::new_in(blocks, frame);
    let none = bits_in(blocks, frame);
    let analysis =
        InitAnalysis::run(ctx, &data, body, &live, &unreached, &none, &mut effects, frame);
    let dead_unwinds = analysis.dead_unwinds(ctx, &data, body, &mut effects, frame);
    let analysis = match dead_unwinds.is_empty() {
        true => analysis,
        false => InitAnalysis::run(
            ctx,
            &data,
            body,
            &live,
            &unreached,
            &dead_unwinds,
            &mut effects,
            frame,
        ),
    };
    Analysed { data, analysis, effects }
}

pub(super) fn elaborate_drops<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &mut MirBody<'db>,
    frame: &Frame,
) -> Result<(), MirLowerError<'db>> {
    let Analysed { data, analysis, mut effects } = analysed(ctx, body, frame);
    let len = data.paths.len();

    // What the blocks do to the paths, and the state at each drop, from the body as it is.
    let mut sites = Vec::new_in(frame);
    // (block, statement after which the flags are written, writes)
    let mut writes: Vec<(BasicBlockId, usize, FlagWrite), &Frame> = Vec::new_in(frame);
    // (block, target of its terminator, the paths that edge initializes in `edge_paths`)
    let mut edge_writes: Vec<(BasicBlockId, BasicBlockId, Range<u32>), &Frame> =
        Vec::new_in(frame);
    let mut edge_paths: Vec<PathId, &Frame> = Vec::new_in(frame);
    let mut dead_drops = Vec::new_in(frame);
    let mut state = InitState::bottom(len, frame);
    for (id, block) in body.basic_blocks.iter() {
        let Some(entry) = &analysis.entry[block_index(id)] else {
            if let Some(TerminatorKind::Drop { target, .. }) =
                block.terminator.as_ref().map(|it| &it.kind)
            {
                dead_drops.push((id, *target));
            }
            continue;
        };
        state.copy_from(entry);
        for at in 0..=block.statements.len() {
            if at == block.statements.len()
                && let Some(Terminator {
                    kind: TerminatorKind::Drop { place, target, unwind },
                    span,
                }) = &block.terminator
            {
                sites.push(DropSite {
                    block: id,
                    place: place.clone(), // clone: an interned place, a refcount
                    target: *target,
                    unwind: *unwind,
                    is_cleanup: block.is_cleanup,
                    span: *span,
                    state: state.copy_in(frame),
                });
                break;
            }
            for &effect in effects.at(ctx, &data, body, id, at) {
                state.apply(data.subtree(effect.path), effect.change);
                let value = match effect.change {
                    Change::Init => true,
                    Change::Uninit => false,
                    Change::Borrowed => continue,
                };
                writes.push((id, at, FlagWrite { path: effect.path, value }));
            }
        }
        if let Some(terminator) = &block.terminator {
            for to in successors(&terminator.kind) {
                let start = edge_paths.len() as u32;
                edge_effects(ctx, &data, body, id, to, |path| edge_paths.push(path));
                let end = edge_paths.len() as u32;
                if end != start {
                    edge_writes.push((id, to, start..end));
                }
            }
        }
    }
    for (id, target) in dead_drops {
        body.basic_blocks[id].terminator.as_mut().expect("a terminator").kind =
            TerminatorKind::Goto { target };
    }

    let mut elaborator = Elaborator {
        ctx,
        body,
        data: &data,
        frame,
        flags: frame.alloc_slice_fill_with(len, |_| None),
        flag_count: 0,
        state: InitState::bottom(len, frame),
        span: MirSpan::Unknown,
        resume: None,
    };
    // rustc makes every flag before it elaborates a drop (`collect_drop_flags`): for each drop
    // in block order, each path under the dropped one that may and may not hold a value there,
    // depth first and the child made last first, as rustc links a path's children. The flags
    // are locals in that order, which is where an `async` block keeps them.
    let mut stack = Vec::new_in(frame);
    for site in &sites {
        let ty = site.place.as_ref().ty(elaborator.body, &ctx.infcx, ctx.env).ty;
        if !ctx.needs_drop(ty) {
            continue;
        }
        let Lookup::Exact(path) = data.lookup(ctx, &site.place) else { continue };
        stack.clear();
        stack.push(path);
        while let Some(at) = stack.pop() {
            if site.state.of(at) == (true, true) {
                elaborator.flag(at);
            }
            stack.extend(data.paths[at].children.iter().map(|&(_, child)| child));
        }
    }
    // After a drop the place holds nothing, whichever way the drop went.
    let mut resets: Vec<(BasicBlockId, PathId), &Frame> = Vec::new_in(frame);
    for site in sites {
        elaborator.state = site.state;
        elaborator.span = site.span;
        let ty = site.place.as_ref().ty(elaborator.body, &ctx.infcx, ctx.env).ty;
        let path = match data.lookup(ctx, &site.place) {
            _ if !ctx.needs_drop(ty) => None,
            Lookup::Exact(path) => Some(path),
            // A part of a value that has no path of its own, as what a reference points to
            // or an element at a runtime index: it is assigned to, so it holds a value.
            Lookup::Untracked => {
                elaborator.patch(site.block, |kind| {
                    *kind = TerminatorKind::Drop {
                        place: site.place,
                        target: site.target,
                        unwind: site.unwind,
                    }
                });
                continue;
            }
        };
        let Some(path) = path else {
            elaborator
                .patch(site.block, |kind| *kind = TerminatorKind::Goto { target: site.target });
            continue;
        };
        let unwind = match (site.is_cleanup, site.unwind) {
            (true, _) => Unwind::InCleanup,
            (false, Some(unwind)) => Unwind::To(unwind),
            (false, None) => Unwind::To(elaborator.resume_block()),
        };
        let succ = elaborator.goto_block(site.target, false);
        resets.push((succ, path));
        let unwind = match unwind {
            Unwind::To(target) => {
                let reset = elaborator.goto_block(target, true);
                resets.push((reset, path));
                Unwind::To(reset)
            }
            Unwind::InCleanup => Unwind::InCleanup,
        };
        let start = elaborator.elaborate(site.place, path, succ, unwind)?;
        elaborator.patch(site.block, |kind| *kind = TerminatorKind::Goto { target: start });
    }

    let Elaborator { flags, flag_count, body, .. } = elaborator;
    if flag_count == 0 {
        return Ok(());
    }
    let flags: &[Option<LocalId>] = flags;
    let bool_ty = ctx.bool_ty();
    // Each kept list of statements below is grown once, to its final size.
    for &(block, path) in &resets {
        let paths = data.subtree(path);
        let block = &mut body.basic_blocks[block];
        block.statements.reserve_exact(count(flags, paths));
        set(&mut block.statements, flags, paths, false, bool_ty);
    }
    for (from, to, range) in edge_writes {
        let paths = &edge_paths[range.start as usize..range.end as usize];
        let n: usize = paths.iter().map(|&path| count(flags, data.subtree(path))).sum();
        if n == 0 {
            continue;
        }
        let mut statements = Vec::with_capacity(n);
        for &path in paths {
            set(&mut statements, flags, data.subtree(path), true, bool_ty);
        }
        let is_cleanup = body.basic_blocks[from].is_cleanup;
        let edge = body.basic_blocks.alloc(BasicBlock {
            statements,
            terminator: Some(Terminator {
                span: MirSpan::Unknown,
                kind: TerminatorKind::Goto { target: to },
            }),
            is_cleanup,
        });
        let terminator = body.basic_blocks[from].terminator.as_mut().expect("a terminator");
        match &mut terminator.kind {
            TerminatorKind::Call { target: Some(target), .. } if *target == to => *target = edge,
            TerminatorKind::Yield { resume, .. } if *resume == to => *resume = edge,
            _ => unreachable!("only a call or a yield initializes on an edge"),
        }
    }
    // Written after the statement, or before the terminator: a statement that panics
    // unwinds with the paths as they were before it. The writes of one statement keep their
    // order, which is the order of its effects. `grouped` is each (block, statement) with the
    // end of its writes in `run`; the writes of one block are next to each other.
    let mut grouped: Vec<((BasicBlockId, usize), u32), &Frame> = Vec::new_in(frame);
    let mut run: Vec<Statement, &Frame> = Vec::new_in(frame);
    for (block, at, write) in writes {
        set(&mut run, flags, data.subtree(write.path), write.value, bool_ty);
        let end = run.len() as u32;
        match grouped.last_mut() {
            Some((key, last)) if *key == (block, at) => *last = end,
            _ => grouped.push(((block, at), end)),
        }
    }
    let mut from = 0;
    let mut block_total: Option<(BasicBlockId, usize)> = None;
    for &((block, _), end) in &grouped {
        let n = end as usize - from;
        from = end as usize;
        match &mut block_total {
            Some((current, total)) if *current == block => *total += n,
            _ => {
                if let Some((current, total)) = block_total {
                    body.basic_blocks[current].statements.reserve_exact(total);
                }
                block_total = Some((block, n));
            }
        }
    }
    if let Some((current, total)) = block_total {
        body.basic_blocks[current].statements.reserve_exact(total);
    }
    // From the back, so the indices of what is still to splice hold, and each drain takes the
    // tail of `run`.
    for group in (0..grouped.len()).rev() {
        let ((block, at), end) = grouped[group];
        let from = if group == 0 { 0 } else { grouped[group - 1].1 as usize };
        if from == end as usize {
            continue;
        }
        let block = &mut body.basic_blocks[block];
        let at = (at + 1).min(block.statements.len());
        block.statements.splice(at..at, run.drain(from..end as usize));
    }
    // Every flag starts clear, but those of what the body is given.
    let mut n = flag_count;
    for param in body.entry_locals() {
        if let Some(path) = data.for_local(param) {
            n += count(flags, data.subtree(path));
        }
    }
    let mut start = Vec::with_capacity(n);
    start.extend(flags.iter().filter_map(|&it| it).map(|flag| write_flag(flag, false, bool_ty)));
    for param in body.entry_locals() {
        if let Some(path) = data.for_local(param) {
            set(&mut start, flags, data.subtree(path), true, bool_ty);
        }
    }
    body.start_block = body.basic_blocks.alloc(BasicBlock {
        statements: start,
        terminator: Some(Terminator {
            span: MirSpan::Unknown,
            kind: TerminatorKind::Goto { target: body.start_block },
        }),
        is_cleanup: false,
    });
    Ok(())
}

/// How many of `paths` have a flag.
fn count(flags: &[Option<LocalId>], paths: &[PathId]) -> usize {
    paths.iter().filter(|&&path| flags[path].is_some()).count()
}

/// Writes `value` to the flag of each of `paths` that has one, in their order.
fn set<'db>(
    out: &mut impl Extend<Statement>,
    flags: &[Option<LocalId>],
    paths: &[PathId],
    value: bool,
    bool_ty: Ty<'db>,
) {
    out.extend(
        paths
            .iter()
            .filter_map(|&path| flags[path])
            .map(|flag| write_flag(flag, value, bool_ty)),
    );
}

fn write_flag(flag: LocalId, value: bool, bool_ty: Ty<'_>) -> Statement {
    // alloc: the constant's one byte, in the kept body
    let value = Operand::from_bytes(Box::new([value as u8]), bool_ty);
    Statement {
        kind: StatementKind::Assign(Place::from(flag).store(), Rvalue::Use(value)),
        span: MirSpan::Unknown,
    }
}

impl<'c, 'f, 'db> Elaborator<'_, 'c, 'f, 'db> {
    fn patch(&mut self, block: BasicBlockId, f: impl FnOnce(&mut TerminatorKind)) {
        f(&mut self.body.basic_blocks[block].terminator.as_mut().expect("a terminator").kind)
    }

    fn new_block(
        &mut self,
        unwind: Unwind,
        statements: Vec<Statement>,
        kind: TerminatorKind,
    ) -> BasicBlockId {
        self.body.basic_blocks.alloc(BasicBlock {
            statements,
            terminator: Some(Terminator { span: self.span, kind }),
            is_cleanup: unwind.is_cleanup(),
        })
    }

    fn goto_block(&mut self, target: BasicBlockId, is_cleanup: bool) -> BasicBlockId {
        self.body.basic_blocks.alloc(BasicBlock {
            statements: Vec::new(), // alloc: empty, no allocation; the resets grow it once
            terminator: Some(Terminator { span: self.span, kind: TerminatorKind::Goto { target } }),
            is_cleanup,
        })
    }

    fn resume_block(&mut self) -> BasicBlockId {
        if let Some(resume) = self.resume {
            return resume;
        }
        let resume = self.body.basic_blocks.alloc(BasicBlock {
            statements: Vec::new(), // alloc: empty, no allocation
            terminator: Some(Terminator {
                span: MirSpan::Unknown,
                kind: TerminatorKind::UnwindResume,
            }),
            is_cleanup: true,
        });
        self.resume = Some(resume);
        resume
    }

    fn new_temp(&mut self, ty: Ty<'db>) -> LocalId {
        self.body.locals.alloc(Local { ty: ty.store() })
    }

    fn flag(&mut self, path: PathId) -> LocalId {
        if let Some(flag) = self.flags[path] {
            return flag;
        }
        let flag = self.new_temp(self.ctx.bool_ty());
        self.flags[path] = Some(flag);
        self.flag_count += 1;
        flag
    }

    fn place_ty(&self, place: &StoredPlace) -> Ty<'db> {
        self.ctx.normalize(place.as_ref().ty(self.body, &self.ctx.infcx, self.ctx.env).ty)
    }

    fn style(&self, path: PathId, deep: bool) -> Style {
        let (init, uninit, multipart) = if deep {
            let subtree = self.data.subtree(path);
            let (mut init, mut uninit) = (false, false);
            for &child in subtree {
                let (i, u) = self.state.of(child);
                init |= i;
                uninit |= u;
            }
            (init, uninit, subtree.len() != 1)
        } else {
            let (init, uninit) = self.state.of(path);
            (init, uninit, false)
        };
        match (init, uninit, multipart) {
            (false, _, _) => Style::Dead,
            (true, false, _) => Style::Static,
            (true, true, false) => Style::Conditional,
            (true, true, true) => Style::Open,
        }
    }

    /// The drop of `place`, which is `path` and every path below it.
    fn elaborate(
        &mut self,
        place: StoredPlace,
        path: PathId,
        succ: BasicBlockId,
        unwind: Unwind,
    ) -> Result<BasicBlockId, MirLowerError<'db>> {
        Ok(match self.style(path, true) {
            Style::Dead => succ,
            Style::Static => self.drop_block(place, succ, unwind),
            Style::Conditional => self.complete_drop(place, path, succ, unwind),
            Style::Open => self.open_drop(place, path, succ, unwind)?,
        })
    }

    fn drop_block(
        &mut self,
        place: StoredPlace,
        succ: BasicBlockId,
        unwind: Unwind,
    ) -> BasicBlockId {
        self.new_block(
            unwind,
            Vec::new(), // alloc: empty, no allocation
            TerminatorKind::Drop { place, target: succ, unwind: unwind.action() },
        )
    }

    /// A drop of all of `place` if the flag of `path` is set.
    fn complete_drop(
        &mut self,
        place: StoredPlace,
        path: PathId,
        succ: BasicBlockId,
        unwind: Unwind,
    ) -> BasicBlockId {
        let drop = self.drop_block(place, succ, unwind);
        self.flag_test_block(path, drop, succ, unwind)
    }

    /// Goes to `on_set` if `path` itself holds a value, and to `on_unset` if not.
    fn flag_test_block(
        &mut self,
        path: PathId,
        on_set: BasicBlockId,
        on_unset: BasicBlockId,
        unwind: Unwind,
    ) -> BasicBlockId {
        if on_set == on_unset {
            return on_set;
        }
        match self.style(path, false) {
            Style::Dead => on_unset,
            Style::Static => on_set,
            Style::Conditional | Style::Open => {
                let flag = self.flag(path);
                self.new_block(
                    unwind,
                    Vec::new(), // alloc: empty, no allocation
                    TerminatorKind::SwitchInt {
                        discr: Operand {
                            kind: OperandKind::Copy(Place::from(flag).store()),
                            span: None,
                        },
                        targets: SwitchTargets::static_if(0, on_unset, on_set),
                    },
                )
            }
        }
    }

    /// The drop of a part of `parent`: of its own path if it has one, and with `parent`
    /// otherwise.
    fn drop_subpath(
        &mut self,
        place: StoredPlace,
        path: Option<PathId>,
        parent: PathId,
        succ: BasicBlockId,
        unwind: Unwind,
    ) -> Result<BasicBlockId, MirLowerError<'db>> {
        match path {
            Some(path) => self.elaborate(place, path, succ, unwind),
            None => Ok(self.complete_drop(place, parent, succ, unwind)),
        }
    }

    /// Drops `fields` in order. If one unwinds, the ones after it are dropped on the way out.
    /// The block that starts the drops, and the one that does when unwinding.
    fn drop_ladder(
        &mut self,
        mut fields: Vec<(StoredPlace, Option<PathId>), &'f Frame>,
        parent: PathId,
        succ: BasicBlockId,
        unwind: Unwind,
    ) -> Result<(BasicBlockId, Unwind), MirLowerError<'db>> {
        fields.retain(|(place, _)| {
            let ty = self.place_ty(place);
            self.ctx.needs_drop(ty)
        });
        let frame = self.frame;
        let mut in_cleanup = Vec::with_capacity_in(fields.len() + 1, frame);
        in_cleanup.resize(fields.len() + 1, Unwind::InCleanup);
        let unwind_ladder = match unwind {
            Unwind::To(target) => {
                let half = self.drop_halfladder(&in_cleanup, target, &fields, parent)?;
                let mut ladder = Vec::with_capacity_in(half.len(), frame);
                ladder.extend(half.iter().map(|&it| Unwind::To(it)));
                ladder
            }
            Unwind::InCleanup => in_cleanup,
        };
        let normal = self.drop_halfladder(&unwind_ladder, succ, &fields, parent)?;
        Ok((*normal.last().expect("a ladder"), *unwind_ladder.last().expect("a ladder")))
    }

    fn drop_halfladder(
        &mut self,
        unwind_ladder: &[Unwind],
        mut succ: BasicBlockId,
        fields: &[(StoredPlace, Option<PathId>)],
        parent: PathId,
    ) -> Result<Vec<BasicBlockId, &'f Frame>, MirLowerError<'db>> {
        let mut ladder = Vec::with_capacity_in(fields.len() + 1, self.frame);
        ladder.push(succ);
        for ((place, path), &unwind) in fields.iter().rev().zip(unwind_ladder) {
            // clone: an interned place, a refcount
            succ = self.drop_subpath(place.clone(), *path, parent, succ, unwind)?;
            ladder.push(succ);
        }
        Ok(ladder)
    }

    fn project(place: &StoredPlace, elem: PlaceElem) -> StoredPlace {
        let place = place.as_ref();
        Place {
            local: place.local,
            projection: Projection::new_from_iter(
                place.projection.as_slice().iter().copied().chain([elem]),
            ),
        }
        .store()
    }

    /// The fields of a struct, tuple, closure or variant at `base`, with their own paths below
    /// `path` if they have one.
    fn fields(
        &self,
        base: &StoredPlace,
        count: usize,
        path: Option<PathId>,
    ) -> Vec<(StoredPlace, Option<PathId>), &'f Frame> {
        let mut fields = Vec::with_capacity_in(count, self.frame);
        fields.extend((0..count).map(|at| {
            let field = FieldIndex(at as u32);
            let place = Self::project(base, ProjectionElem::Field(field));
            let sub = path.and_then(|path| self.data.find_child(path, Step::Field(field)));
            (place, sub)
        }));
        fields
    }

    fn open_drop(
        &mut self,
        place: StoredPlace,
        path: PathId,
        succ: BasicBlockId,
        unwind: Unwind,
    ) -> Result<BasicBlockId, MirLowerError<'db>> {
        let ty = self.place_ty(&place);
        match ty.kind() {
            TyKind::Tuple(tys) => {
                let fields = self.fields(&place, tys.len(), Some(path));
                Ok(self.drop_ladder(fields, path, succ, unwind)?.0)
            }
            TyKind::Closure(_, args) => {
                let count = args.as_closure().tupled_upvars_ty().tuple_fields().len();
                let fields = self.fields(&place, count, Some(path));
                Ok(self.drop_ladder(fields, path, succ, unwind)?.0)
            }
            TyKind::Adt(adt, _) if adt.is_box() => {
                self.open_drop_for_box(place, path, ty, succ, unwind)
            }
            TyKind::Adt(adt, _) if adt.is_enum() => {
                let hir_def::AdtId::EnumId(id) = adt.def_id() else { unreachable!() };
                self.open_drop_for_enum(place, path, id, succ, unwind)
            }
            TyKind::Adt(adt, _) if !adt.is_union() => {
                let variant = hir_def::VariantId::from_non_enum(adt.def_id())
                    .expect("a struct has one variant");
                let count = self.ctx.db.field_types(variant).iter().count();
                let fields = self.fields(&place, count, Some(path));
                Ok(self.drop_ladder(fields, path, succ, unwind)?.0)
            }
            TyKind::Array(_, len) => match try_const_usize(self.ctx.db, len) {
                Some(len) => self.open_drop_for_array(place, path, len as u64, succ, unwind),
                None => Ok(self.complete_drop(place, path, succ, unwind)),
            },
            _ => Ok(self.complete_drop(place, path, succ, unwind)),
        }
    }

    /// The contents first, then `<Box<T, A> as Drop>::drop`, which frees them, then the
    /// allocator. The box itself is freed if what holds it is not moved out of, even when
    /// its contents are.
    fn open_drop_for_box(
        &mut self,
        place: StoredPlace,
        path: PathId,
        ty: Ty<'db>,
        succ: BasicBlockId,
        unwind: Unwind,
    ) -> Result<BasicBlockId, MirLowerError<'db>> {
        let free = self.box_free_block(&place, path, ty, succ, unwind)?;
        let free_unwind = match unwind {
            Unwind::To(target) => {
                Unwind::To(self.box_free_block(&place, path, ty, target, Unwind::InCleanup)?)
            }
            Unwind::InCleanup => Unwind::InCleanup,
        };
        let contents = Self::project(&place, ProjectionElem::Deref);
        let contents_path = self.data.find_child(path, Step::BoxContents);
        self.drop_subpath(contents, contents_path, path, free, free_unwind)
    }

    /// `<Box<T, A> as Drop>::drop`, then the fields of the box, if the box itself holds a value.
    fn box_free_block(
        &mut self,
        place: &StoredPlace,
        path: PathId,
        ty: Ty<'db>,
        succ: BasicBlockId,
        unwind: Unwind,
    ) -> Result<BasicBlockId, MirLowerError<'db>> {
        let TyKind::Adt(adt, _) = ty.kind() else { unreachable!("a box is an ADT") };
        let variant = hir_def::VariantId::from_non_enum(adt.def_id()).expect("a box is a struct");
        let count = self.ctx.db.field_types(variant).iter().count();
        let mut fields = Vec::with_capacity_in(count, self.frame);
        fields.extend(
            (0..count)
                .map(|at| Self::project(place, ProjectionElem::Field(FieldIndex(at as u32))))
                .filter(|place| self.ctx.needs_drop(self.place_ty(place))),
        );
        // The fields are dropped as they are, under the test of the flag of the box below.
        let mut fields_succ = succ;
        let mut fields_unwind = unwind;
        for field in fields.iter().rev() {
            let unwind_here = fields_unwind;
            // clone: an interned place, a refcount
            fields_succ = self.drop_block(field.clone(), fields_succ, unwind_here);
            if let Unwind::To(target) = fields_unwind {
                // clone: an interned place, a refcount
                let cleanup = self.drop_block(field.clone(), target, Unwind::InCleanup);
                fields_unwind = Unwind::To(cleanup);
            }
        }
        let destructor = self.destructor_call_block(place, ty, fields_succ, fields_unwind)?;
        Ok(self.flag_test_block(path, destructor, succ, unwind))
    }

    /// `<ty as Drop>::drop(&mut place)`.
    fn destructor_call_block(
        &mut self,
        place: &StoredPlace,
        ty: Ty<'db>,
        succ: BasicBlockId,
        unwind: Unwind,
    ) -> Result<BasicBlockId, MirLowerError<'db>> {
        let interner = self.ctx.infcx.interner;
        let Some(drop_fn) = interner.lang_items().Drop_drop else {
            return Err(MirLowerError::LangItemNotFound);
        };
        let reference =
            Ty::new_ref(interner, Region::new_erased(interner), ty, rustc_ast_ir::Mutability::Mut);
        let reference = self.new_temp(reference);
        let unit = self.new_temp(Ty::new_unit(interner));
        let borrow = Statement {
            kind: StatementKind::Assign(
                Place::from(reference).store(),
                // clone: an interned place, a refcount
                Rvalue::Ref(BorrowKind::Mut { kind: MutBorrowKind::Default }, place.clone()),
            ),
            span: self.span,
        };
        let func =
            Operand::from_fn(self.ctx.db, drop_fn, GenericArgs::new_from_slice(&[ty.into()]));
        let call = TerminatorKind::Call {
            func,
            // alloc: the call's one argument, in the kept body
            args: Box::new([Operand {
                kind: OperandKind::Move(Place::from(reference).store()),
                span: None,
            }]),
            destination: Place::from(unit).store(),
            target: Some(succ),
            cleanup: unwind.action(),
            from_hir_call: false,
            intrinsic: None,
        };
        // alloc: the one statement of a kept block, at its size
        Ok(self.new_block(unwind, vec![borrow], call))
    }

    /// Switches on the variant. A variant that has a part moved out of it is dropped field by
    /// field; the others are dropped whole, if the enum itself holds a value.
    fn open_drop_for_enum(
        &mut self,
        place: StoredPlace,
        path: PathId,
        id: hir_def::EnumId,
        succ: BasicBlockId,
        unwind: Unwind,
    ) -> Result<BasicBlockId, MirLowerError<'db>> {
        let db = self.ctx.db;
        let frame = self.frame;
        let mut variants = Vec::new_in(frame);
        variants.extend(id.enum_variants(db).variants.values().map(|&(variant, _)| variant));
        let mut values = Vec::with_capacity_in(variants.len(), frame);
        let mut blocks = Vec::with_capacity_in(variants.len() + 1, frame);
        let mut otherwise = false;
        let mut otherwise_needs_drop = false;
        for &variant in variants.iter() {
            let count = db.field_types(variant.into()).iter().count();
            match self.data.find_child(path, Step::Variant(variant.into())) {
                Some(variant_path) => {
                    let base = Self::project(&place, ProjectionElem::Downcast(variant.into()));
                    let mut fields = self.fields(&base, count, None);
                    for (at, field) in fields.iter_mut().enumerate() {
                        field.1 =
                            self.data.find_child(variant_path, Step::Field(FieldIndex(at as u32)));
                    }
                    let discr = db
                        .const_eval_discriminant(variant)
                        // alloc: error path
                        .map_err(|e| MirLowerError::NotSupported(format!("discriminant: {e:?}")))?;
                    values.push(discr as u128);
                    blocks.push(self.drop_ladder(fields, path, succ, unwind)?.0);
                }
                None => {
                    otherwise = true;
                    let base = Self::project(&place, ProjectionElem::Downcast(variant.into()));
                    otherwise_needs_drop |= self
                        .fields(&base, count, None)
                        .iter()
                        .any(|(field, _)| self.ctx.needs_drop(self.place_ty(field)));
                }
            }
        }
        if !otherwise {
            values.pop();
        } else if !otherwise_needs_drop {
            blocks.push(succ);
        } else {
            // clone: an interned place, a refcount
            blocks.push(self.drop_block(place.clone(), succ, unwind));
        }
        let last = *blocks.last().expect("an enum with a variant to drop");
        // Include the otherwise target. Reading the discriminant when every edge leads
        // to the same drop would keep a moved-out enum alive across a suspension point.
        if blocks.iter().all(|&block| block == last) {
            return Ok(self.flag_test_block(path, last, succ, unwind));
        }
        let discr = self.new_temp(Ty::new_int(self.ctx.infcx.interner, rustc_type_ir::IntTy::I128));
        let read = Statement {
            kind: StatementKind::Assign(Place::from(discr).store(), Rvalue::Discriminant(place)),
            span: self.span,
        };
        let switch = self.new_block(
            unwind,
            // alloc: the one statement of a kept block, at its size
            vec![read],
            TerminatorKind::SwitchInt {
                discr: Operand { kind: OperandKind::Copy(Place::from(discr).store()), span: None },
                targets: SwitchTargets::new(
                    values.iter().copied().zip(blocks.iter().copied()),
                    last,
                ),
            },
        );
        Ok(self.flag_test_block(path, switch, succ, unwind))
    }

    /// The elements that have a path of their own one by one, and the runs between them each
    /// as one array, from the back.
    fn open_drop_for_array(
        &mut self,
        place: StoredPlace,
        path: PathId,
        len: u64,
        succ: BasicBlockId,
        unwind: Unwind,
    ) -> Result<BasicBlockId, MirLowerError<'db>> {
        enum Part {
            Run(u64, u64),
            Element(u64, PathId),
        }
        let frame = self.frame;
        let mut parts = Vec::new_in(frame);
        let mut run_start = Some(0);
        for at in 0..len {
            match self.data.find_child(path, Step::Element(at)) {
                Some(element) => {
                    if let Some(start) = run_start.take() {
                        parts.push(Part::Run(start, at));
                    }
                    parts.push(Part::Element(at, element));
                }
                None => {
                    run_start.get_or_insert(at);
                }
            }
        }
        if let Some(start) = run_start {
            parts.push(Part::Run(start, len));
        }
        let mut fields = Vec::with_capacity_in(parts.len(), frame);
        fields.extend(
            parts
                .iter()
                .rev()
                .filter(|part| !matches!(part, Part::Run(start, end) if start == end))
                .map(|part| match *part {
                    Part::Run(start, end) => (
                        Self::project(
                            &place,
                            ProjectionElem::Subslice { from: start, to: len - end },
                        ),
                        None,
                    ),
                    Part::Element(at, element) => (
                        Self::project(
                            &place,
                            ProjectionElem::ConstantIndex {
                                offset: at,
                                min_length: at + 1,
                                from_end: false,
                            },
                        ),
                        Some(element),
                    ),
                }),
        );
        Ok(self.drop_ladder(fields, path, succ, unwind)?.0)
    }
}
