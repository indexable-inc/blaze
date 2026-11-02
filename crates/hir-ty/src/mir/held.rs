//! What an `async` block keeps in its future from one `poll` to the next, computed as rustc's
//! coroutine transform computes it on the runtime body (`rustc_mir_transform/src/coroutine/
//! layout.rs` at 1.99.0: `locals_live_across_suspend_points`, `compute_storage_conflicts` and
//! the variant fields of `compute_layout`), with the analyses it runs (`MaybeBorrowedLocals`,
//! `MaybeRequiresStorage`, `MaybeLiveLocals` and `always_storage_live_locals` of
//! `rustc_mir_dataflow`). Given the same body, the same locals are kept in the same variants
//! with the same conflicts, and `rustc_abi`'s coroutine layout places them as rustc does.
//!
//! rustc's runtime body has had its `FakeRead`s removed (`CleanupPostBorrowck`); they are
//! ignored here. No statement unwinds: what panics is an `Assert` before it.

use blaze_ids::bits::{BitSliceMut, Bits, Ones, words_for};
use blaze_mem::Frame;

use super::{
    AsmOperand, BasicBlockId, BorrowKind, LocalId, MirBody, Operand, OperandKind, ProjectionElem,
    Rvalue, Statement, StatementKind, StoredPlace, Terminator, TerminatorKind,
};

/// The locals an `async` block keeps in its future: rustc's `CoroutineSavedLocal`s.
#[derive(Debug, PartialEq, Eq)]
pub struct HeldLocals {
    /// The locals live across some yield, in the order the body declares them.
    pub locals: Box<[LocalId]>,
    /// The places the body yields at, in the order of its blocks: the states of the future
    /// after the three rustc reserves (not started, returned, panicked).
    pub yields: Box<[YieldPoint]>,
    /// Whether two of the locals may need their storage at once: per local a row of
    /// `locals.len().div_ceil(64)` words, a bit per local.
    conflicts: Box<[u64]>,
}

/// One place the body yields at, which is one variant of the future.
#[derive(Debug, PartialEq, Eq)]
pub struct YieldPoint {
    pub resume: BasicBlockId,
    /// Where a future suspended here is dropped.
    pub drop: Option<BasicBlockId>,
    /// The fields of the variant: the held locals live here, by their index in
    /// [`HeldLocals::locals`].
    pub kept: Box<[usize]>,
}

impl HeldLocals {
    pub fn contains(&self, local: LocalId) -> bool {
        self.locals.binary_search(&local).is_ok()
    }

    /// Whether the `a`th and the `b`th of the locals may need their storage at once, and so
    /// can't share memory.
    pub fn conflict(&self, a: usize, b: usize) -> bool {
        let row = row(&self.conflicts, self.locals.len(), a);
        (row[b / 64] >> (b % 64)) & 1 != 0
    }

    /// The locals the `a`th may need its storage at once with, by their index in
    /// [`HeldLocals::locals`], in order.
    pub fn conflicts_of(&self, a: usize) -> impl Iterator<Item = usize> + '_ {
        Ones::<usize>::new(row(&self.conflicts, self.locals.len(), a))
    }
}

/// The `a`th row of `n` locals' conflicts.
fn row(conflicts: &[u64], n: usize, a: usize) -> &[u64] {
    let words = n.div_ceil(64);
    &conflicts[a * words..(a + 1) * words]
}

/// A set of locals, a bit each, its words in the run's frame: rustc's `DenseBitSet` without a
/// heap block per set. Every set of a run keeps the domain it was made with, and its bits past
/// the domain stay clear.
type Set<'f> = BitSliceMut<'f>;

/// The empty set of `domain` locals in `frame`.
fn empty_in(frame: &Frame, domain: usize) -> Set<'_> {
    Bits::from_words(frame.alloc_slice_fill_with(words_for(domain), |_| 0))
}

/// The set of all `domain` locals in `frame`.
fn filled_in(frame: &Frame, domain: usize) -> Set<'_> {
    let mut set = empty_in(frame, domain);
    set.insert_range(0..domain);
    set
}

/// A copy of `set` in `frame`.
fn copy_in<'g>(set: &Set<'_>, frame: &'g Frame) -> Set<'g> {
    Bits::from_words(frame.alloc_slice_copy(set.words()))
}

fn local_index(local: LocalId) -> usize {
    local.into_raw().into_u32() as usize
}

fn block_index(block: BasicBlockId) -> usize {
    block.into_raw().into_u32() as usize
}

fn is_indirect(place: &StoredPlace) -> bool {
    place.projection.as_slice().contains(&ProjectionElem::Deref)
}

fn index_locals(place: &StoredPlace) -> impl Iterator<Item = usize> + '_ {
    place.projection.as_slice().iter().filter_map(|elem| match elem {
        ProjectionElem::Index(local) => Some(local_index(*local)),
        _ => None,
    })
}

fn operand_place(operand: &Operand) -> Option<&StoredPlace> {
    match &operand.kind {
        OperandKind::Copy(place) | OperandKind::Move(place) => Some(place),
        _ => None,
    }
}

/// The edges out of a block, each with what it does on its own: a call's return writes its
/// destination, a resumed yield its resume argument.
#[derive(Clone, Copy)]
enum Edge<'a> {
    Plain(BasicBlockId),
    Returns(BasicBlockId, &'a StoredPlace),
    Resumes(BasicBlockId, &'a StoredPlace),
}

impl Edge<'_> {
    fn to(self) -> BasicBlockId {
        match self {
            Edge::Plain(to) | Edge::Returns(to, _) | Edge::Resumes(to, _) => to,
        }
    }
}

/// Calls `each` with the edges out of a block that go to a block the body reaches, in order.
fn edges<'a>(body: &MirBody<'_>, terminator: &'a Terminator, mut each: impl FnMut(Edge<'a>)) {
    let mut edge = |it: Edge<'a>| {
        if !body.unreached.contains(&it.to()) {
            each(it);
        }
    };
    match &terminator.kind {
        TerminatorKind::Call { target, destination, .. } => {
            for &to in target.iter() {
                edge(Edge::Returns(to, destination));
            }
        }
        TerminatorKind::Yield { resume, resume_arg, drop, .. } => {
            edge(Edge::Resumes(*resume, resume_arg));
            for &to in drop.iter() {
                edge(Edge::Plain(to));
            }
        }
        kind => {
            for to in kind.successors() {
                edge(Edge::Plain(to));
            }
        }
    }
    if let Some(to) = terminator.kind.unwind() {
        edge(Edge::Plain(to));
    }
}

/// The blocks control reaches from the start.
fn reachable<'f>(frame: &'f Frame, body: &MirBody<'_>) -> &'f [bool] {
    let blocks = body.basic_blocks.len();
    let reached = frame.alloc_slice_fill_with(blocks, |_| false);
    let mut stack = Vec::with_capacity_in(blocks, frame);
    stack.push(body.start_block);
    while let Some(id) = stack.pop() {
        if std::mem::replace(&mut reached[block_index(id)], true) {
            continue;
        }
        if let Some(terminator) = &body.basic_blocks[id].terminator {
            edges(body, terminator, |edge| stack.push(edge.to()));
        }
    }
    reached
}

/// `MaybeBorrowedLocals`: a local some pointer may point into. `Drop` counts, because a
/// destructor is handed `&mut self`.
fn borrow_statement(statement: &StatementKind, state: &mut Set<'_>) {
    match statement {
        StatementKind::Assign(_, Rvalue::Ref(kind, place))
            if *kind != BorrowKind::Shallow && !is_indirect(place) =>
        {
            state.insert(local_index(place.local));
        }
        StatementKind::StorageDead(local) => {
            state.remove(local_index(*local));
        }
        _ => (),
    }
}

fn borrow_terminator(terminator: &TerminatorKind, state: &mut Set<'_>) {
    if let TerminatorKind::Drop { place, .. } | TerminatorKind::DropAndReplace { place, .. } =
        terminator
        && !is_indirect(place)
    {
        state.insert(local_index(place.local));
    }
}

/// A local an operand moves out of as a whole stops requiring storage after it, unless it was
/// borrowed before (`MaybeRequiresStorage`'s primary effect).
fn kill_whole_move(operand: &Operand, borrowed: &Set<'_>, requires: &mut Set<'_>) {
    if let OperandKind::Move(place) = &operand.kind
        && place.projection.as_slice().is_empty()
    {
        let local = local_index(place.local);
        if !borrowed.contains(local) {
            requires.remove(local);
        }
    }
}

/// Calls `each` with the places an inline `asm!` writes: its register outputs, then what it
/// takes.
fn asm_outputs<'a>(statement: &'a StatementKind, mut each: impl FnMut(&'a StoredPlace)) {
    if let StatementKind::InlineAsm(asm) = statement {
        for operand in &asm.operands {
            if let AsmOperand::Reg { output: Some(output), .. } = operand {
                each(output);
            }
        }
        for place in asm.taken.iter() {
            each(place);
        }
    }
}

/// The forward analyses: `MaybeBorrowedLocals` and `MaybeRequiresStorage` at the entry of
/// each block, `None` for a block not reached yet.
struct Forward<'f> {
    borrowed: &'f mut [Option<Set<'f>>],
    requires: &'f mut [Option<Set<'f>>],
}

impl Forward<'_> {
    /// Loads the entry states of block `at` into the buffers, which keep their memory.
    fn load(&self, at: usize, borrowed: &mut Set<'_>, requires: &mut Set<'_>) {
        load_one(&self.borrowed[at], borrowed);
        load_one(&self.requires[at], requires);
    }
}

fn load_one(known: &Option<Set<'_>>, into: &mut Set<'_>) {
    match known {
        Some(known) => into.copy_from(known),
        None => into.clear(),
    }
}

/// Joins `new` into the state an edge flows to; says whether that changed it.
fn join<'f>(frame: &'f Frame, known: &mut Option<Set<'f>>, new: &Set<'_>) -> bool {
    match known {
        Some(known) => known.union_with(new),
        None => {
            *known = Some(copy_in(new, frame));
            true
        }
    }
}

/// Flows the states at the end of a block along an edge to `to`, queueing `to` if they changed
/// its entry states.
fn flow<'f>(
    frame: &'f Frame,
    forward: &mut Forward<'f>,
    worklist: &mut Vec<BasicBlockId, &'f Frame>,
    to: BasicBlockId,
    borrowed: &Set<'_>,
    requires: &Set<'_>,
) {
    let at = block_index(to);
    let changed =
        join(frame, &mut forward.borrowed[at], borrowed) | join(frame, &mut forward.requires[at], requires);
    if changed {
        worklist.push(to);
    }
}

impl MirBody<'_> {
    /// The locals an `async` block keeps in its future, as rustc's coroutine transform finds
    /// them. The body is the runtime one, its drops elaborated. For a caller outside any query;
    /// one inside a query uses [`MirBody::held_locals_in`] with its frame.
    pub fn locals_held_across_yields(&self) -> HeldLocals {
        blaze_mem::frame::task(crate::db::MIR_TAG, |frame| self.held_locals_in(frame))
    }

    /// [`MirBody::locals_held_across_yields`], its analyses in `frame`.
    pub fn held_locals_in(&self, frame: &Frame) -> HeldLocals {
        let count = self.locals.len();
        let reached = reachable(frame, self);
        let always_live = self.always_storage_live(frame);
        // A coroutine's first parameter is its receiver, the future itself: none of its locals.
        let forward = self.forward(frame, reached, self.param_locals.get(1..).unwrap_or(&[]));
        let live_out = self.liveness(frame, reached);
        let receiver = self.param_locals.first().copied().map(local_index);

        let mut any = empty_in(frame, count);
        let mut at_yields = Vec::new_in(frame);
        let mut borrowed = empty_in(frame, count);
        let mut requires = empty_in(frame, count);
        for (id, block) in self.basic_blocks.iter() {
            let Some(Terminator { kind: TerminatorKind::Yield { resume, drop, .. }, .. }) =
                &block.terminator
            else {
                continue;
            };
            if !reached[block_index(id)] {
                continue;
            }
            forward.load(block_index(id), &mut borrowed, &mut requires);
            for statement in &block.statements {
                self.storage_statement(statement, &mut borrowed, &mut requires, &mut |_| ());
            }
            let mut live = copy_in(&live_out[block_index(id)], frame);
            // An `async` block can't be moved once polled, so a borrow may outlive the use
            // of what it points at: a borrowed local is live until its storage ends.
            live.union_with(&borrowed);
            live.intersect_with(&requires);
            if let Some(receiver) = receiver {
                live.remove(receiver);
            }
            // What the body uses in place of a capture is the future's own field, and a
            // promoted constant is no local of rustc's.
            for &(local, _) in &self.upvars_in_place {
                live.remove(local_index(local));
            }
            for &local in &self.promoted_constants {
                live.remove(local_index(local));
            }
            any.union_with(&live);
            at_yields.push((*resume, *drop, live));
        }

        // alloc: the result's held locals, one block at its size
        let mut locals = Vec::with_capacity(any.count());
        locals.extend(
            self.locals.iter().map(|(local, _)| local).filter(|&it| any.contains(local_index(it))),
        );
        let saved: &[usize] = frame.alloc_slice_from_iter(locals.iter().map(|&it| local_index(it)));
        // alloc: the result's yield points, one block at its size
        let mut yields = Vec::with_capacity(at_yields.len());
        yields.extend(at_yields.iter().map(|(resume, drop, live)| {
            // `live` is a subset of `any`, so this is its size.
            // alloc: the variant's fields, one block at its size
            let mut kept = Vec::with_capacity(live.count());
            kept.extend((0..saved.len()).filter(|&at| live.contains(saved[at])));
            YieldPoint { resume: *resume, drop: *drop, kept: kept.into_boxed_slice() }
        }));
        let conflicts = self.storage_conflicts(frame, reached, &forward, &always_live, &any, saved);
        HeldLocals {
            locals: locals.into_boxed_slice(),
            yields: yields.into_boxed_slice(),
            conflicts,
        }
    }

    /// The locals held across the call that ends each block of `points`, as
    /// [`MirBody::held_locals_in`] finds them across a yield: live after the call (the call's
    /// own destination not), or borrowed before it, while their storage is required (after the
    /// call's whole moves of its operands). The first parameter is an ordinary one here (not
    /// removed as a coroutine's receiver is); promoted constants are removed as there. One slice
    /// per point, in order, each in the order the body declares its locals; an unreached
    /// block's is empty.
    pub fn held_across_calls_in<'f>(
        &self,
        frame: &'f Frame,
        points: &[BasicBlockId],
    ) -> &'f [&'f [LocalId]] {
        let count = self.locals.len();
        let reached = reachable(frame, self);
        let forward = self.forward(frame, reached, &self.param_locals);
        let live_out = self.liveness(frame, reached);
        let mut borrowed = empty_in(frame, count);
        let mut requires = empty_in(frame, count);
        let mut held = Vec::new_in(frame);
        frame.alloc_slice_fill_with(points.len(), |at| {
            let id = points[at];
            if !reached[block_index(id)] {
                return &[][..];
            }
            let block = &self.basic_blocks[id];
            forward.load(block_index(id), &mut borrowed, &mut requires);
            for statement in &block.statements {
                self.storage_statement(statement, &mut borrowed, &mut requires, &mut |_| ());
            }
            if let Some(terminator) = &block.terminator {
                let kind = &terminator.kind;
                kind.for_each_operand(&mut |op| kill_whole_move(op, &borrowed, &mut requires));
                if let TerminatorKind::Call { destination, .. } = kind {
                    requires.remove(local_index(destination.local));
                }
            }
            let mut live = copy_in(&live_out[block_index(id)], frame);
            live.union_with(&borrowed);
            live.intersect_with(&requires);
            for &local in &self.promoted_constants {
                live.remove(local_index(local));
            }
            held.clear();
            held.extend(
                self.locals.iter().map(|(local, _)| local).filter(|&it| live.contains(local_index(it))),
            );
            &*frame.alloc_slice_copy(&held)
        })
    }

    /// `always_storage_live_locals`: the locals no `StorageLive` or `StorageDead` mentions.
    fn always_storage_live<'f>(&self, frame: &'f Frame) -> Set<'f> {
        let mut always = filled_in(frame, self.locals.len());
        for (_, block) in self.basic_blocks.iter() {
            for statement in &block.statements {
                if let StatementKind::StorageLive(local) | StatementKind::StorageDead(local) =
                    statement.kind
                {
                    always.remove(local_index(local));
                }
            }
        }
        always
    }

    /// Both forward analyses over one statement. `early` is told of the storage required
    /// after the statement's early effect, which is what storage conflicts are taken from.
    fn storage_statement(
        &self,
        statement: &Statement,
        borrowed: &mut Set<'_>,
        requires: &mut Set<'_>,
        early: &mut impl FnMut(&Set<'_>),
    ) {
        let kind = &statement.kind;
        // Early effect: a borrowed or assigned local needs its storage for the statement.
        borrow_statement(kind, requires);
        match kind {
            StatementKind::StorageDead(local) => {
                requires.remove(local_index(*local));
            }
            StatementKind::Assign(place, _) => {
                requires.insert(local_index(place.local));
            }
            _ => (),
        }
        asm_outputs(kind, |output| {
            requires.insert(local_index(output.local));
        });
        early(requires);
        // Primary effect: a local moved out of as a whole, and never borrowed before, needs
        // no storage after.
        kind.for_each_operand(&mut |op| kill_whole_move(op, borrowed, requires));
        if let StatementKind::Assign(place, _) = kind {
            requires.insert(local_index(place.local));
        }
        asm_outputs(kind, |output| {
            requires.insert(local_index(output.local));
        });
        borrow_statement(kind, borrowed);
    }

    /// Both forward analyses over a terminator, up to its edges.
    fn storage_terminator(
        &self,
        terminator: &TerminatorKind,
        borrowed: &mut Set<'_>,
        requires: &mut Set<'_>,
        early: &mut impl FnMut(&Set<'_>),
    ) {
        borrow_terminator(terminator, requires);
        if let TerminatorKind::Call { destination, .. } = terminator {
            requires.insert(local_index(destination.local));
        }
        early(requires);
        if let TerminatorKind::Call { destination, .. } = terminator {
            requires.remove(local_index(destination.local));
        }
        terminator.for_each_operand(&mut |op| kill_whole_move(op, borrowed, requires));
        borrow_terminator(terminator, borrowed);
    }

    /// `params`: the parameters whose storage the start block requires, the ones the body
    /// reads its arguments from.
    fn forward<'f>(&self, frame: &'f Frame, reached: &[bool], params: &[LocalId]) -> Forward<'f> {
        let count = self.locals.len();
        let blocks = self.basic_blocks.len();
        let mut forward = Forward {
            borrowed: frame.alloc_slice_fill_with(blocks, |_| None),
            requires: frame.alloc_slice_fill_with(blocks, |_| None),
        };
        let mut start_requires = empty_in(frame, count);
        for &param in params {
            start_requires.insert(local_index(param));
        }
        let start = block_index(self.start_block);
        forward.borrowed[start] = Some(empty_in(frame, count));
        forward.requires[start] = Some(start_requires);
        let mut worklist = Vec::with_capacity_in(blocks, frame);
        worklist.push(self.start_block);
        // Scratch states, reused by every block and edge: no allocation per visit.
        let mut borrowed = empty_in(frame, count);
        let mut requires = empty_in(frame, count);
        let mut edge_requires = empty_in(frame, count);
        while let Some(id) = worklist.pop() {
            if !reached[block_index(id)] {
                continue;
            }
            forward.load(block_index(id), &mut borrowed, &mut requires);
            let block = &self.basic_blocks[id];
            for statement in &block.statements {
                self.storage_statement(statement, &mut borrowed, &mut requires, &mut |_| ());
            }
            let Some(terminator) = &block.terminator else { continue };
            self.storage_terminator(&terminator.kind, &mut borrowed, &mut requires, &mut |_| ());
            edges(self, terminator, |edge| match edge {
                Edge::Plain(to) => {
                    flow(frame, &mut forward, &mut worklist, to, &borrowed, &requires)
                }
                Edge::Returns(to, place) | Edge::Resumes(to, place) => {
                    edge_requires.copy_from(&requires);
                    edge_requires.insert(local_index(place.local));
                    flow(frame, &mut forward, &mut worklist, to, &borrowed, &edge_requires);
                }
            });
        }
        forward
    }

    /// `MaybeLiveLocals`: the locals live at the end of each block, after the effects its
    /// edges have on their own.
    fn liveness<'f>(&self, frame: &'f Frame, reached: &[bool]) -> &'f [Set<'f>] {
        let count = self.locals.len();
        let blocks = self.basic_blocks.len();
        let live_in = frame.alloc_slice_fill_with(blocks, |_| empty_in(frame, count));
        let live_out = frame.alloc_slice_fill_with(blocks, |_| empty_in(frame, count));
        // Scratch states, reused by every block and edge; a changed block's state is swapped
        // into `live_in` and its old one becomes the next scratch.
        let mut state = empty_in(frame, count);
        let mut entry = empty_in(frame, count);
        let mut changed = true;
        while changed {
            changed = false;
            for (id, block) in self.basic_blocks.iter().rev() {
                if !reached[block_index(id)] {
                    continue;
                }
                state.clear();
                if let Some(terminator) = &block.terminator {
                    edges(self, terminator, |edge| match edge {
                        Edge::Plain(to) => {
                            state.union_with(&live_in[block_index(to)]);
                        }
                        Edge::Returns(to, place) => {
                            entry.copy_from(&live_in[block_index(to)]);
                            if place.projection.as_slice().is_empty() {
                                entry.remove(local_index(place.local));
                            }
                            state.union_with(&entry);
                        }
                        Edge::Resumes(to, place) => {
                            entry.copy_from(&live_in[block_index(to)]);
                            def_use_write(place, &mut entry);
                            insert_all(&mut entry, index_locals(place));
                            state.union_with(&entry);
                        }
                    });
                }
                live_out[block_index(id)].copy_from(&state);
                if let Some(terminator) = &block.terminator {
                    live_terminator(&terminator.kind, &mut state);
                }
                for statement in block.statements.iter().rev() {
                    live_statement(&statement.kind, &mut state);
                }
                if live_in[block_index(id)] != state {
                    std::mem::swap(&mut live_in[block_index(id)], &mut state);
                    changed = true;
                }
            }
        }
        live_out
    }

    /// `compute_storage_conflicts`: two held locals conflict if both need their storage at
    /// some statement or terminator of a block that does not end in `Unreachable`. One that
    /// is always storage live conflicts with every other.
    fn storage_conflicts(
        &self,
        frame: &Frame,
        reached: &[bool],
        forward: &Forward<'_>,
        always_live: &Set<'_>,
        held: &Set<'_>,
        saved: &[usize],
    ) -> Box<[u64]> {
        let count = self.locals.len();
        let mut matrix = Conflicts::new(frame, count, always_live, held, saved);
        let mut apply = |state: &Set<'_>| matrix.apply(state);
        let mut borrowed = empty_in(frame, count);
        let mut requires = empty_in(frame, count);
        for (id, block) in self.basic_blocks.iter() {
            if !reached[block_index(id)] {
                continue;
            }
            let Some(terminator) = &block.terminator else { continue };
            if let TerminatorKind::Unreachable = terminator.kind {
                continue;
            }
            forward.load(block_index(id), &mut borrowed, &mut requires);
            for statement in &block.statements {
                self.storage_statement(statement, &mut borrowed, &mut requires, &mut apply);
            }
            self.storage_terminator(&terminator.kind, &mut borrowed, &mut requires, &mut apply);
        }
        matrix.finish()
    }
}

/// The storage conflicts of the held locals, over their positions in `saved` (every local of
/// `held`, in order): `saved.len()²` bits, where rustc's `BitMatrix` has a row of every local
/// per local. Each state costs a word-wise intersection and one row union per held local in it.
struct Conflicts<'a> {
    held: &'a Set<'a>,
    /// A local's position in `saved`, `NOT_HELD` for one not held.
    position: &'a mut [u32],
    /// The held locals that are always storage live, by position: they conflict with all.
    ineligible: Set<'a>,
    together: &'a mut [Set<'a>],
    live_held: Set<'a>,
    eligible: Set<'a>,
}

const NOT_HELD: u32 = u32::MAX;

impl<'a> Conflicts<'a> {
    fn new(
        frame: &'a Frame,
        count: usize,
        always_live: &Set<'_>,
        held: &'a Set<'a>,
        saved: &[usize],
    ) -> Conflicts<'a> {
        let n = saved.len();
        let position = frame.alloc_slice_fill_with(count, |_| NOT_HELD);
        let mut ineligible = empty_in(frame, n);
        for (at, &local) in saved.iter().enumerate() {
            position[local] = at as u32;
            if always_live.contains(local) {
                ineligible.insert(at);
            }
        }
        let together = frame.alloc_slice_fill_with(n, |_| copy_in(&ineligible, frame));
        Conflicts {
            held,
            position,
            ineligible,
            together,
            live_held: empty_in(frame, count),
            eligible: empty_in(frame, n),
        }
    }

    /// The locals that need their storage at one point all conflict with each other.
    fn apply(&mut self, state: &Set<'_>) {
        self.live_held.copy_from(state);
        self.live_held.intersect_with(self.held);
        self.eligible.clear();
        for local in self.live_held.iter() {
            debug_assert_ne!(self.position[local], NOT_HELD);
            self.eligible.insert(self.position[local] as usize);
        }
        for at in self.eligible.iter() {
            self.together[at].union_with(&self.eligible);
        }
    }

    /// The conflicts as [`HeldLocals`] keeps them: per held local a row of `n.div_ceil(64)`
    /// words, made once at its size.
    fn finish(self) -> Box<[u64]> {
        let n = self.together.len();
        let words = n.div_ceil(64);
        if words == 0 {
            return Box::default();
        }
        // alloc: the result's conflict rows, one block at its size
        let mut conflicts = vec![0u64; n * words].into_boxed_slice();
        for (at, row) in conflicts.chunks_exact_mut(words).enumerate() {
            if self.ineligible.contains(at) {
                BitSliceMut::<usize>::from_words(row).insert_range(0..n);
            } else {
                row.copy_from_slice(self.together[at].words());
            }
        }
        conflicts
    }
}

/// A write to `place` as liveness sees it (`DefUse::for_place` of a store): the whole local is
/// defined, a write through a pointer uses the pointer, a write to a part is neither.
fn def_use_write(place: &StoredPlace, state: &mut Set<'_>) {
    if is_indirect(place) {
        state.insert(local_index(place.local));
    } else if place.projection.as_slice().is_empty() {
        state.remove(local_index(place.local));
    }
}

fn insert_all(state: &mut Set<'_>, locals: impl Iterator<Item = usize>) {
    for local in locals {
        state.insert(local);
    }
}

fn use_place(place: &StoredPlace, state: &mut Set<'_>) {
    state.insert(local_index(place.local));
    insert_all(state, index_locals(place));
}

fn use_operand(operand: &Operand, state: &mut Set<'_>) {
    if let Some(place) = operand_place(operand) {
        use_place(place, state);
    }
}

/// `MaybeLiveLocals` backwards over a statement: what it writes is defined first, then what it
/// reads is used.
fn live_statement(statement: &StatementKind, state: &mut Set<'_>) {
    match statement {
        StatementKind::Assign(place, rvalue) => {
            def_use_write(place, state);
            insert_all(state, index_locals(place));
            match rvalue {
                Rvalue::Ref(_, from)
                | Rvalue::Len(from)
                | Rvalue::Discriminant(from)
                | Rvalue::CopyForDeref(from) => use_place(from, state),
                _ => rvalue.for_each_operand(&mut |it| use_operand(it, state)),
            }
        }
        StatementKind::Deinit(place) => {
            def_use_write(place, state);
            insert_all(state, index_locals(place));
        }
        StatementKind::InlineAsm(_) => {
            asm_outputs(statement, |output| {
                def_use_write(output, state);
                insert_all(state, index_locals(output));
            });
            statement.for_each_operand(&mut |it| use_operand(it, state));
        }
        StatementKind::FakeRead(_)
        | StatementKind::StorageLive(_)
        | StatementKind::StorageDead(_)
        | StatementKind::Nop => (),
    }
}

/// `MaybeLiveLocals` backwards over a terminator. A call's destination and a yield's resume
/// argument are written on the edges, not here.
fn live_terminator(terminator: &TerminatorKind, state: &mut Set<'_>) {
    match terminator {
        TerminatorKind::Call { destination, .. } => {
            if is_indirect(destination) {
                state.insert(local_index(destination.local));
            }
            insert_all(state, index_locals(destination));
        }
        TerminatorKind::Drop { place, .. } => use_place(place, state),
        TerminatorKind::DropAndReplace { place, .. } => {
            def_use_write(place, state);
            use_place(place, state);
        }
        _ => (),
    }
    terminator.for_each_operand(&mut |it| use_operand(it, state));
}

#[cfg(test)]
mod tests {
    use blaze_ids::rng::SplitMix64;
    use blaze_mem::{Frame, Tag, frame};
    use rustc_index::bit_set::DenseBitSet;

    use super::{Conflicts, Ones, Set, empty_in, filled_in, row};

    fn dense(count: usize, members: &[usize]) -> DenseBitSet<usize> {
        let mut set = DenseBitSet::new_empty(count);
        for &it in members {
            set.insert(it);
        }
        set
    }

    fn of<'f>(frame: &'f Frame, count: usize, members: &[usize]) -> Set<'f> {
        let mut set = empty_in(frame, count);
        for &it in members {
            set.insert(it);
        }
        set
    }

    /// The members of a set of `count` locals, each in it one time in `one_in`, in order.
    fn members(rng: &mut SplitMix64, count: usize, one_in: u64) -> Vec<usize> {
        (0..count).filter(|_| rng.one_in(one_in)).collect()
    }

    /// The matrix as it was computed before [`Conflicts`]: a row of every local per local, in
    /// rustc's `DenseBitSet`s.
    fn model(count: usize, always_live: &[usize], held: &[usize], states: &[Vec<usize>]) -> Vec<bool> {
        let held_set = dense(count, held);
        let mut ineligible = dense(count, always_live);
        ineligible.intersect(&held_set);
        let mut together: Vec<DenseBitSet<usize>> = vec![ineligible.clone(); count];
        for state in states {
            let mut eligible = dense(count, state);
            eligible.intersect(&held_set);
            for local in eligible.iter() {
                together[local].union(&eligible);
            }
        }
        let n = held.len();
        let mut conflicts = vec![false; n * n];
        for (a, &local_a) in held.iter().enumerate() {
            for (b, &local_b) in held.iter().enumerate() {
                conflicts[a * n + b] =
                    ineligible.contains(local_a) || together[local_a].contains(local_b);
            }
        }
        conflicts
    }

    #[test]
    fn conflicts_over_held_positions_are_the_full_matrix() {
        let mut rng = SplitMix64::new(0x9e37_79b9_7f4a_7c15);
        for round in 0..200 {
            let count = [0, 1, 2, 63, 64, 65, 130][round % 7] + rng.index(40);
            let density = 1 + rng.below(4);
            let held = members(&mut rng, count, density);
            let density = 1 + rng.below(8);
            let always_live = members(&mut rng, count, density);
            let mut states = Vec::new();
            for _ in 0..rng.below(12) {
                let density = 1 + rng.below(5);
                states.push(members(&mut rng, count, density));
            }
            let expected = model(count, &always_live, &held, &states);
            let got = frame::task(Tag::Mir, |frame| {
                let held_set = of(frame, count, &held);
                let always = of(frame, count, &always_live);
                let mut matrix = Conflicts::new(frame, count, &always, &held_set, &held);
                for state in &states {
                    matrix.apply(&of(frame, count, state));
                }
                matrix.finish()
            });
            let n = held.len();
            for a in 0..n {
                let row = row(&got, n, a);
                let listed: Vec<usize> = Ones::<usize>::new(row).collect();
                let wanted: Vec<usize> = (0..n).filter(|&b| expected[a * n + b]).collect();
                assert_eq!(listed, wanted, "round {round}: {count} locals, {n} held, row {a}");
            }
        }
    }

    #[test]
    fn frame_sets_are_dense_bit_sets() {
        let mut rng = SplitMix64::new(0x2545_f491_4f6c_dd1d);
        for round in 0..300 {
            let count = [0, 1, 63, 64, 65, 129][round % 6] + rng.index(70);
            let density = 1 + rng.below(4);
            let a = members(&mut rng, count, density);
            let density = 1 + rng.below(4);
            let b = members(&mut rng, count, density);
            frame::task(Tag::Mir, |frame| {
                let (mut x, y) = (of(frame, count, &a), of(frame, count, &b));
                let (mut dx, dy) = (dense(count, &a), dense(count, &b));
                assert_eq!(x.union_with(&y), dx.union(&dy), "round {round}");
                assert_eq!(x.iter().collect::<Vec<_>>(), dx.iter().collect::<Vec<_>>(), "round {round}");
                assert_eq!(x.count(), dx.count(), "round {round}");
                x.intersect_with(&of(frame, count, &a));
                dx.intersect(&dense(count, &a));
                assert_eq!(x.iter().collect::<Vec<_>>(), dx.iter().collect::<Vec<_>>(), "round {round}");
                assert_eq!(x == of(frame, count, &a), dx == dense(count, &a), "round {round}");
                let filled = filled_in(frame, count);
                assert_eq!(
                    filled.iter().collect::<Vec<_>>(),
                    DenseBitSet::<usize>::new_filled(count).iter().collect::<Vec<_>>(),
                    "round {round}",
                );
            });
        }
    }
}
