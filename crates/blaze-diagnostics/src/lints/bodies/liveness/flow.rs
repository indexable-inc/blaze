//! The dataflow of rustc's liveness lints over one MIR body: which of the places they check
//! may be read later at each point, every assignment to those places with whether its value
//! is, and which of them are ever live or dropped.
//!
//! Follows rustc's `compiler/rustc_mir_transform/src/liveness.rs` (`PlaceSet::get`,
//! `MaybeLivePlaces`, `TransferFunction`, `DefUse`, `find_dead_assignments`,
//! `is_local_in_reachable_code`), `compiler/rustc_mir_dataflow/src/framework/mod.rs`
//! (`iterate_to_fixpoint`: the work queue in postorder) and
//! `compiler/rustc_middle/src/mir/traversal.rs` (`Postorder`) (MIT or Apache-2.0).

use std::{collections::VecDeque, rc::Rc};

use blaze_hash::HashMap;
use blaze_ids::bits::{BitMatrix, BitSet};
use hir_ty::mir::{
    AggregateKind, AsmOperand, BasicBlockId, LocalId, MirBody, MirSpan, Operand, OperandKind,
    PlaceElem, ProjectionElem, Rvalue, StatementKind, StoredPlace, TerminatorKind,
};
use hir_ty::next_solver::Ty;
use smallvec::SmallVec;

/// An index of a checked place: rustc's `PlaceIndex`.
pub(super) type Index = u32;

const NONE: Index = Index::MAX;

/// A set of checked places: rustc's `DenseBitSet<PlaceIndex>`.
pub(super) type PlaceSet = BitSet<Index>;

/// What a checked place is: rustc's `LocalInfo` of its local, or a capture.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    /// A binding (`BindingForm::Var`).
    Var(hir_def::hir::BindingId),
    /// The reference a match guard reads a by-value binding through (`RefForGuard`), with
    /// the place of the binding.
    Guard(Index),
    /// A capture of the closure the body is of, by its field, and whether by reference.
    Capture { field: u32, by_ref: bool },
}

/// The places the lints check: rustc's `PlaceSet`.
pub(super) struct Places<'db> {
    pub(super) kinds: Vec<Kind>,
    pub(super) tys: Vec<Ty<'db>>,
    /// The checked place of each local, by its index, [`NONE`] for the others: of a binding,
    /// a guard's reference, and the local lowering keeps a capture in.
    locals: Vec<Index>,
    /// The closure's own local when the closure is borrowed, with the place of each capture
    /// it owns, which the body uses in place (`(*_1).f`).
    in_closure: Option<(LocalId, SmallVec<[Index; 4]>)>,
}

/// The tables of [`Places`], [`Cfg`] and the sightings, kept from body to body.
#[derive(Default)]
pub(super) struct Tables<'db> {
    kinds: Vec<Kind>,
    tys: Vec<Ty<'db>>,
    locals: Vec<Index>,
    succ: Vec<SmallVec<[BasicBlockId; 2]>>,
    pred: Vec<SmallVec<[BasicBlockId; 2]>>,
    postorder: Vec<BasicBlockId>,
    stack: Vec<(BasicBlockId, usize)>,
    pub(super) sightings: Vec<Sighting<'db>>,
}

pub(super) fn raw_local(local: LocalId) -> usize {
    local.into_raw().into_u32() as usize
}

fn raw(block: BasicBlockId) -> usize {
    block.into_raw().into_u32() as usize
}

impl<'db> Places<'db> {
    pub(super) fn new(locals: usize, tables: &mut Tables<'db>) -> Places<'db> {
        let mut kinds = std::mem::take(&mut tables.kinds);
        kinds.clear();
        let mut tys = std::mem::take(&mut tables.tys);
        tys.clear();
        let mut by_local = std::mem::take(&mut tables.locals);
        by_local.clear();
        by_local.resize(locals, NONE);
        Places { kinds, tys, locals: by_local, in_closure: None }
    }

    /// Gives the tables back for the next body.
    pub(super) fn give_back(self, tables: &mut Tables<'db>) {
        tables.kinds = self.kinds;
        tables.tys = self.tys;
        tables.locals = self.locals;
    }

    pub(super) fn len(&self) -> usize {
        self.kinds.len()
    }

    /// Adds a place, of `local` when it is kept in one.
    pub(super) fn push(&mut self, kind: Kind, ty: Ty<'db>, local: Option<LocalId>) -> Index {
        let at = self.kinds.len() as Index;
        self.kinds.push(kind);
        self.tys.push(ty);
        if let Some(local) = local {
            self.locals[raw_local(local)] = at;
        }
        at
    }

    /// Makes the capture `at`, the field `field` of the borrowed closure `closure`, used in
    /// place.
    pub(super) fn in_closure(&mut self, closure: LocalId, field: u32, at: Index) {
        let (_, fields) = self.in_closure.get_or_insert_with(|| (closure, SmallVec::new()));
        let field = field as usize;
        if fields.len() <= field {
            fields.resize(field + 1, NONE);
        }
        fields[field] = at;
    }

    pub(super) fn of_local(&self, local: LocalId) -> Option<Index> {
        self.locals.get(raw_local(local)).copied().filter(|&it| it != NONE)
    }

    /// The checked place `local` with `projection` is in, and the projections past it:
    /// rustc's `PlaceSet::get`. A capture by reference is the place behind the reference.
    fn get<'p>(&self, local: LocalId, projection: &'p [PlaceElem]) -> Option<(Index, &'p [PlaceElem])> {
        if let Some(at) = self.of_local(local) {
            return match self.kinds[at as usize] {
                Kind::Capture { by_ref: true, .. } => match projection {
                    [ProjectionElem::Deref, rest @ ..] => Some((at, rest)),
                    _ => None,
                },
                _ => Some((at, projection)),
            };
        }
        let (closure, fields) = self.in_closure.as_ref()?;
        if *closure != local {
            return None;
        }
        let [ProjectionElem::Deref, ProjectionElem::Field(field), rest @ ..] = projection else { return None };
        let at = *fields.get(field.0 as usize)?;
        (at != NONE).then_some((at, rest))
    }

    /// Whether `place` is behind a reference out of a checked place: rustc's self-assignments
    /// of one are not.
    pub(super) fn is_indirect(&self, place: &StoredPlace) -> bool {
        self.get(place.local, place.projection.as_slice()).is_some_and(|(_, extra)| is_indirect(extra))
    }

    fn is_by_ref_capture(&self, at: Index) -> bool {
        matches!(self.kinds[at as usize], Kind::Capture { by_ref: true, .. })
    }
}

/// What the body is of, as rustc's `CaptureKind`: what decides when what a closure borrows
/// is read by its caller.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CaptureKind {
    None,
    Fn,
    FnMut,
    FnOnce,
    Coroutine,
    CoroutineClosure,
}

/// A statement the transfer function reads differently.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Special {
    /// Neither a use nor an assignment: a `FakeRead` of what a `let` just bound
    /// (`ForLet(None)`), and the copy lowering makes of a capture into a local.
    Skip,
    /// `t = x OP y` of a self-assignment `x = x OP y` (or `x OP= y`): reads only `y`.
    SelfOp,
    /// `x = t` of a self-assignment, which is not a definition of `x`.
    SelfStore,
}

/// A location in the body: a statement, or the terminator at the block's length.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Loc {
    pub(super) block: BasicBlockId,
    pub(super) index: u32,
}

/// What reading a place in a context does to its liveness: rustc's `DefUse::for_place`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ctx {
    /// An assignment of the place.
    Store,
    /// What a call, a yield or an asm block writes.
    Call,
    /// A read, a borrow, a move.
    Use,
}

/// The dead captures of a closure, as the closure's analysis decides: rustc's result of
/// `check_liveness`, by field; `None` when the closure could not be analysed, which keeps
/// every capture live.
pub(super) type DeadCaptures = Option<Rc<[bool]>>;

/// The body's control flow, with rustc's `FnMut` closure that is called again after it
/// returns or unwinds (`Goto { target: START_BLOCK }`).
pub(super) struct Cfg {
    succ: Vec<SmallVec<[BasicBlockId; 2]>>,
    pred: Vec<SmallVec<[BasicBlockId; 2]>>,
    pub(super) postorder: Vec<BasicBlockId>,
    pub(super) start: BasicBlockId,
}

impl Cfg {
    /// The blocks reachable from the start, by every edge rustc's MIR has (unwinding, a
    /// match's false edges), but none into a block rustc builds no way into.
    pub(super) fn new(mir: &MirBody<'_>, again: bool, tables: &mut Tables<'_>) -> Cfg {
        let n = mir.basic_blocks.len();
        let start = mir.start_block;
        let mut unreached: BitSet = BitSet::new(n);
        for &it in &mir.unreached {
            unreached.insert(raw(it));
        }
        let mut succ = std::mem::take(&mut tables.succ);
        succ.clear();
        succ.resize_with(n, SmallVec::new);
        for (block, data) in mir.basic_blocks.iter() {
            let Some(terminator) = &data.terminator else { continue };
            let edges: SmallVec<[BasicBlockId; 2]> = match &terminator.kind {
                // alloc: inline, a `SmallVec` of one allocates nothing
                TerminatorKind::Return | TerminatorKind::UnwindResume if again => smallvec::smallvec![start],
                TerminatorKind::FalseEdge { real_target, imaginary_target } => {
                    // alloc: inline, a `SmallVec` of two allocates nothing
                    smallvec::smallvec![*real_target, *imaginary_target]
                }
                // alloc: into a `SmallVec` of two, inline but for a switch of more targets
                kind => kind.successors().into_iter().chain(kind.unwind()).collect(),
            };
            succ[raw(block)].extend(edges.into_iter().filter(|it| !unreached.contains(raw(*it))));
        }
        let mut visited: BitSet = BitSet::new(n);
        let mut postorder = std::mem::take(&mut tables.postorder);
        postorder.clear();
        let mut stack = std::mem::take(&mut tables.stack);
        stack.clear();
        stack.push((start, succ[raw(start)].len()));
        visited.insert(raw(start));
        while let Some(top) = stack.len().checked_sub(1) {
            let (block, left) = stack[top];
            if left == 0 {
                postorder.push(block);
                stack.pop();
                continue;
            }
            stack[top].1 = left - 1;
            let next = succ[raw(block)][left - 1];
            if visited.insert(raw(next)) {
                stack.push((next, succ[raw(next)].len()));
            }
        }
        tables.stack = stack;
        let mut pred = std::mem::take(&mut tables.pred);
        pred.clear();
        pred.resize_with(n, SmallVec::new);
        for &block in &postorder {
            for &next in &succ[raw(block)] {
                pred[raw(next)].push(block);
            }
        }
        Cfg { succ, pred, postorder, start }
    }

    /// Gives the tables back for the next body.
    pub(super) fn give_back(self, tables: &mut Tables<'_>) {
        tables.succ = self.succ;
        tables.pred = self.pred;
        tables.postorder = self.postorder;
    }

    /// rustc's `Location::is_predecessor_of`: `to` can be reached from `from`.
    pub(super) fn reaches(&self, from: Loc, to: Loc) -> bool {
        if from.block == to.block && from.index < to.index {
            return true;
        }
        let mut visited: BitSet = BitSet::new(self.pred.len());
        // alloc: asked only of a dead assignment found, which is reported
        let mut queue: Vec<BasicBlockId> = self.pred[raw(to.block)].to_vec();
        while let Some(block) = queue.pop() {
            if !visited.insert(raw(block)) {
                continue;
            }
            queue.extend(self.pred[raw(block)].iter().copied());
            if block == from.block {
                return true;
            }
        }
        false
    }
}

/// An assignment of a checked place as rustc's `find_dead_assignments` sees it.
pub(super) struct Sighting<'m> {
    pub(super) at: Index,
    pub(super) kind: Access,
    pub(super) loc: Loc,
    pub(super) live: bool,
    pub(super) direct: bool,
    /// Where it is written, for an assignment of the body.
    pub(super) span: MirSpan,
    /// The place assigned, for an assignment.
    pub(super) place: Option<&'m StoredPlace>,
}

/// rustc's `AccessKind`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Access {
    Param,
    Assign,
    Capture,
}

/// What the dataflow found: rustc's `AssignmentResult` before it reports.
pub(super) struct Seen<'m> {
    pub(super) sightings: Vec<Sighting<'m>>,
    pub(super) ever_live: PlaceSet,
    pub(super) ever_dropped: PlaceSet,
}

/// The liveness of the checked places of one body: rustc's `MaybeLivePlaces`.
pub(super) struct Flow<'m, 'p, 'db> {
    pub(super) mir: &'m MirBody<'db>,
    places: &'p Places<'db>,
    specials: &'p HashMap<(u32, u32), Special>,
    cfg: &'p Cfg,
    kind: CaptureKind,
    /// The state where each block starts, a row per block.
    entry: BitMatrix<Index>,
}

impl<'m, 'p, 'db> Flow<'m, 'p, 'db> {
    pub(super) fn new(
        mir: &'m MirBody<'db>,
        places: &'p Places<'db>,
        specials: &'p HashMap<(u32, u32), Special>,
        cfg: &'p Cfg,
        kind: CaptureKind,
    ) -> Self {
        let entry = BitMatrix::new(mir.basic_blocks.len(), places.len());
        Flow { mir, places, specials, cfg, kind, entry }
    }

    /// Runs the backward analysis to its fixpoint, the blocks first in postorder.
    pub(super) fn solve(&mut self, closures: &mut super::Liveness<'_, '_, 'db>) {
        let n = self.mir.basic_blocks.len();
        let mut queued: BitSet = BitSet::new(n);
        let mut queue: VecDeque<BasicBlockId> = VecDeque::with_capacity(self.cfg.postorder.len());
        for &block in &self.cfg.postorder {
            queued.insert(raw(block));
            queue.push_back(block);
        }
        let mut state = PlaceSet::new(self.places.len());
        while let Some(block) = queue.pop_front() {
            queued.remove(raw(block));
            self.exit(block, &mut state);
            self.block(block, &mut state, closures);
            if state.as_slice() != self.entry.row(raw(block)) {
                self.entry.row_mut(raw(block)).copy_from(&state);
                for &pred in &self.cfg.pred[raw(block)] {
                    if queued.insert(raw(pred)) {
                        queue.push_back(pred);
                    }
                }
            }
        }
    }

    fn exit(&self, block: BasicBlockId, state: &mut PlaceSet) {
        state.clear();
        for &next in &self.cfg.succ[raw(block)] {
            state.union_with(&self.entry.row(raw(next)));
        }
    }

    fn block(&self, block: BasicBlockId, state: &mut PlaceSet, closures: &mut super::Liveness<'_, '_, 'db>) {
        let data = &self.mir.basic_blocks[block];
        if let Some(terminator) = &data.terminator {
            self.terminator(&terminator.kind, state);
        }
        for (index, statement) in data.statements.iter().enumerate().rev() {
            self.statement(&statement.kind, (raw(block) as u32, index as u32), state, closures);
        }
    }

    fn statement(&self, kind: &StatementKind, loc: (u32, u32), live: &mut PlaceSet, closures: &mut super::Liveness<'_, '_, 'db>) {
        let special = self.specials.get(&loc).copied();
        if special == Some(Special::Skip) {
            return;
        }
        match kind {
            StatementKind::Assign(place, rvalue) => match special {
                Some(Special::SelfOp) => {
                    if let Rvalue::BinaryOp(_, _, rhs) | Rvalue::CheckedBinaryOp(_, _, rhs) = rvalue {
                        self.operand(rhs, live);
                    }
                }
                Some(Special::SelfStore) => self.rvalue(rvalue, live, closures),
                _ => {
                    self.place(place, Ctx::Store, live);
                    self.rvalue(rvalue, live, closures);
                }
            },
            StatementKind::FakeRead(place) => self.place(place, Ctx::Use, live),
            StatementKind::Deinit(place) => self.place(place, Ctx::Store, live),
            StatementKind::InlineAsm(asm) => {
                for operand in &asm.operands {
                    match operand {
                        AsmOperand::Reg { input, output, .. } => {
                            if let Some(input) = input {
                                self.operand(input, live);
                            }
                            if let Some(output) = output {
                                self.place(output, Ctx::Call, live);
                            }
                        }
                        AsmOperand::Const(it) | AsmOperand::Sym(it) => self.operand(it, live),
                        AsmOperand::Label => {}
                    }
                }
                if let Some(taken) = &asm.taken {
                    self.place(taken, Ctx::Call, live);
                }
            }
            StatementKind::StorageLive(_) | StatementKind::StorageDead(_) | StatementKind::Nop => {}
        }
    }

    fn terminator(&self, kind: &TerminatorKind, live: &mut PlaceSet) {
        let again = self.kind == CaptureKind::FnMut;
        match kind {
            TerminatorKind::Return | TerminatorKind::Yield { .. } | TerminatorKind::Call { target: None, .. }
                if self.kind != CaptureKind::None =>
            {
                self.by_ref_captures(live)
            }
            TerminatorKind::Goto { target } if *target == self.cfg.start && self.kind != CaptureKind::None => {
                self.by_ref_captures(live)
            }
            TerminatorKind::UnwindResume if again => self.by_ref_captures(live),
            TerminatorKind::Drop { .. } | TerminatorKind::Assert { .. } => {}
            TerminatorKind::DropAndReplace { place, value, .. } => {
                self.place(place, Ctx::Store, live);
                self.operand(value, live);
            }
            TerminatorKind::SwitchInt { discr, .. } => self.operand(discr, live),
            TerminatorKind::Call { func, args, destination, .. } => {
                self.operand(func, live);
                for arg in args.iter() {
                    self.operand(arg, live);
                }
                self.place(destination, Ctx::Call, live);
            }
            // rustc's `super_terminator`: by-ref captures are not marked live at a `become`.
            TerminatorKind::TailCall { func, args, .. } => {
                self.operand(func, live);
                for arg in args.iter() {
                    self.operand(arg, live);
                }
            }
            TerminatorKind::Yield { value, resume_arg, .. } => {
                self.operand(value, live);
                self.place(resume_arg, Ctx::Call, live);
            }
            TerminatorKind::Goto { .. }
            | TerminatorKind::UnwindResume
            | TerminatorKind::Abort
            | TerminatorKind::Return
            | TerminatorKind::Unreachable
            | TerminatorKind::CoroutineDrop
            | TerminatorKind::FalseEdge { .. }
            | TerminatorKind::FalseUnwind { .. } => {}
        }
    }

    /// What the closure's caller can read after it returns, yields or unwinds: what it
    /// captured by reference.
    fn by_ref_captures(&self, live: &mut PlaceSet) {
        for at in 0..self.places.len() as Index {
            if self.places.is_by_ref_capture(at) {
                live.insert(at);
            }
        }
    }

    fn rvalue(&self, rvalue: &Rvalue, live: &mut PlaceSet, closures: &mut super::Liveness<'_, '_, 'db>) {
        match rvalue {
            Rvalue::Use(it) | Rvalue::Repeat(it, _) | Rvalue::Cast(_, it, _) | Rvalue::UnaryOp(_, it) => {
                self.operand(it, live)
            }
            Rvalue::Ref(_, place) | Rvalue::Len(place) | Rvalue::Discriminant(place) | Rvalue::CopyForDeref(place) => {
                self.place(place, Ctx::Use, live)
            }
            Rvalue::BinaryOp(_, lhs, rhs) | Rvalue::CheckedBinaryOp(_, lhs, rhs) => {
                self.operand(lhs, live);
                self.operand(rhs, live);
            }
            Rvalue::Aggregate(AggregateKind::Closure(ty), operands) => match closures.dead_captures(ty.as_ref()) {
                Some(dead) => {
                    for (operand, dead) in operands.iter().zip(dead.iter()) {
                        if !dead {
                            self.operand(operand, live);
                        }
                    }
                }
                None => {
                    for operand in operands.iter() {
                        self.operand(operand, live);
                    }
                }
            },
            Rvalue::Aggregate(_, operands) => {
                for operand in operands.iter() {
                    self.operand(operand, live);
                }
            }
            Rvalue::ThreadLocalRef(never) | Rvalue::AddressOf(never) | Rvalue::NullaryOp(never) => match *never {},
        }
    }

    fn operand(&self, operand: &Operand, live: &mut PlaceSet) {
        if let OperandKind::Copy(place) | OperandKind::Move(place) = &operand.kind {
            self.place(place, Ctx::Use, live);
        }
    }

    /// rustc's `TransferFunction::visit_place`: an index local in the projections is read,
    /// then the place is defined or used as [`def_use`] says.
    fn place(&self, place: &StoredPlace, ctx: Ctx, live: &mut PlaceSet) {
        let projection = place.projection.as_slice();
        for elem in projection {
            if let ProjectionElem::Index(local) = elem
                && let Some(at) = self.places.of_local(*local)
            {
                live.insert(at);
            }
        }
        let Some((at, extra)) = self.places.get(place.local, projection) else { return };
        match def_use(extra, ctx) {
            Some(true) => {
                live.remove(at);
            }
            Some(false) => {
                live.insert(at);
            }
            None => {}
        }
    }

    /// Every assignment of a checked place in the blocks reachable from the start, with its
    /// liveness after it, and of the parameters and captures where the body starts: rustc's
    /// `find_dead_assignments`.
    pub(super) fn assignments(
        &self,
        closures: &mut super::Liveness<'_, '_, 'db>,
        sightings: Vec<Sighting<'m>>,
    ) -> Seen<'m> {
        let n = self.places.len();
        let mut found = Seen { sightings, ever_live: PlaceSet::new(n), ever_dropped: PlaceSet::new(n) };
        let mut state = PlaceSet::new(n);
        let mir: &'m MirBody<'db> = self.mir;
        for &block in &self.cfg.postorder {
            let data = &mir.basic_blocks[block];
            self.exit(block, &mut state);
            found.ever_live.union_with(&state);
            let end = Loc { block, index: data.statements.len() as u32 };
            if let Some(terminator) = &data.terminator {
                let span = terminator.span;
                match &terminator.kind {
                    TerminatorKind::Call { destination: place, .. } | TerminatorKind::Yield { resume_arg: place, .. } => {
                        self.sight(&mut found, place, span, end, &state);
                        self.dropped(&mut found, place);
                    }
                    TerminatorKind::DropAndReplace { place, .. } => {
                        self.sight(&mut found, place, span, end, &state);
                        self.dropped(&mut found, place);
                    }
                    TerminatorKind::Drop { place, .. } => self.dropped(&mut found, place),
                    _ => {}
                }
                self.terminator(&terminator.kind, &mut state);
            }
            for (index, statement) in data.statements.iter().enumerate().rev() {
                found.ever_live.union_with(&state);
                let loc = Loc { block, index: index as u32 };
                let key = (raw(block) as u32, index as u32);
                if self.specials.get(&key) != Some(&Special::Skip) {
                    match &statement.kind {
                        StatementKind::Assign(place, _) => self.sight(&mut found, place, statement.span, loc, &state),
                        StatementKind::InlineAsm(asm) => {
                            for operand in &asm.operands {
                                if let AsmOperand::Reg { output: Some(place), .. } = operand {
                                    self.sight(&mut found, place, statement.span, loc, &state);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                self.statement(&statement.kind, key, &mut state, closures);
            }
        }
        let start = self.entry.row(raw(self.cfg.start));
        found.ever_live.union_with(&start);
        let at_start = Loc { block: self.cfg.start, index: 0 };
        for at in 0..n as Index {
            let kind = match self.places.kinds[at as usize] {
                Kind::Capture { by_ref: true, .. } => continue,
                Kind::Capture { by_ref: false, .. } => Access::Capture,
                Kind::Var(_) if self.is_param(at) => Access::Param,
                _ => continue,
            };
            found.sightings.push(Sighting {
                at,
                kind,
                loc: at_start,
                live: start.contains(at),
                direct: true,
                span: MirSpan::Unknown,
                place: None,
            });
        }
        found
    }

    fn is_param(&self, at: Index) -> bool {
        self.mir.param_locals.iter().any(|&local| self.places.of_local(local) == Some(at))
    }

    fn sight(&self, found: &mut Seen<'m>, place: &'m StoredPlace, span: MirSpan, loc: Loc, live: &PlaceSet) {
        let Some((at, extra)) = self.places.get(place.local, place.projection.as_slice()) else { return };
        if is_indirect(extra) {
            return;
        }
        found.sightings.push(Sighting {
            at,
            kind: Access::Assign,
            loc,
            live: live.contains(at),
            direct: extra.is_empty(),
            span,
            place: Some(place),
        });
    }

    fn dropped(&self, found: &mut Seen<'m>, place: &StoredPlace) {
        if let Some((at, [])) = self.places.get(place.local, place.projection.as_slice()) {
            found.ever_dropped.insert(at);
        }
    }

    /// Whether a block reachable from the start names `local`: rustc's
    /// `is_local_in_reachable_code`.
    pub(super) fn mentions(&self, local: LocalId) -> bool {
        let in_place = |place: &StoredPlace| {
            place.local == local
                || place.projection.as_slice().iter().any(|it| *it == ProjectionElem::Index(local))
        };
        let in_operand = |operand: &Operand| match &operand.kind {
            OperandKind::Copy(place) | OperandKind::Move(place) => in_place(place),
            _ => false,
        };
        let in_rvalue = |rvalue: &Rvalue| match rvalue {
            Rvalue::Use(it) | Rvalue::Repeat(it, _) | Rvalue::Cast(_, it, _) | Rvalue::UnaryOp(_, it) => in_operand(it),
            Rvalue::Ref(_, place) | Rvalue::Len(place) | Rvalue::Discriminant(place) | Rvalue::CopyForDeref(place) => {
                in_place(place)
            }
            Rvalue::BinaryOp(_, lhs, rhs) | Rvalue::CheckedBinaryOp(_, lhs, rhs) => in_operand(lhs) || in_operand(rhs),
            Rvalue::Aggregate(_, operands) => operands.iter().any(in_operand),
            Rvalue::ThreadLocalRef(never) | Rvalue::AddressOf(never) | Rvalue::NullaryOp(never) => match *never {},
        };
        self.cfg.postorder.iter().any(|&block| {
            let data = &self.mir.basic_blocks[block];
            let statements = data.statements.iter().any(|statement| match &statement.kind {
                StatementKind::Assign(place, rvalue) => in_place(place) || in_rvalue(rvalue),
                StatementKind::FakeRead(place) | StatementKind::Deinit(place) => in_place(place),
                StatementKind::StorageLive(it) | StatementKind::StorageDead(it) => *it == local,
                StatementKind::InlineAsm(asm) => {
                    asm.operands.iter().any(|operand| match operand {
                        AsmOperand::Reg { input, output, .. } => {
                            input.as_ref().is_some_and(in_operand) || output.as_ref().is_some_and(in_place)
                        }
                        AsmOperand::Const(it) | AsmOperand::Sym(it) => in_operand(it),
                        AsmOperand::Label => false,
                    }) || asm.taken.as_ref().is_some_and(in_place)
                }
                StatementKind::Nop => false,
            });
            statements
                || data.terminator.as_ref().is_some_and(|terminator| match &terminator.kind {
                    TerminatorKind::SwitchInt { discr, .. } => in_operand(discr),
                    TerminatorKind::Drop { place, .. } => in_place(place),
                    TerminatorKind::DropAndReplace { place, value, .. } => in_place(place) || in_operand(value),
                    TerminatorKind::Call { func, args, destination, .. } => {
                        in_operand(func) || args.iter().any(in_operand) || in_place(destination)
                    }
                    TerminatorKind::TailCall { func, args, .. } => in_operand(func) || args.iter().any(in_operand),
                    TerminatorKind::Assert { cond, .. } => in_operand(cond),
                    TerminatorKind::Yield { value, resume_arg, .. } => in_operand(value) || in_place(resume_arg),
                    _ => false,
                })
        })
    }
}

/// rustc's `DefUse::for_place` of a place past a checked one by `extra`: `Some(true)` a
/// definition, `Some(false)` a use.
fn def_use(extra: &[PlaceElem], ctx: Ctx) -> Option<bool> {
    let indirect = is_indirect(extra);
    match ctx {
        Ctx::Store if indirect => Some(false),
        Ctx::Store => extra.is_empty().then_some(true),
        Ctx::Call => indirect.then_some(false),
        Ctx::Use => Some(false),
    }
}

fn is_indirect(projection: &[PlaceElem]) -> bool {
    projection.iter().any(|it| *it == ProjectionElem::Deref)
}
