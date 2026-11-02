//! What a body's statements say to the analyses, read in one pass: the reachable blocks in
//! reverse postorder, each statement's operands in order, as `MoveData::gather` and
//! `Loans::gather` walked them, written into columns by point and by block. After it, the move
//! paths, liveness and the loans read only the columns.
//!
//! A per-point column is one slice of items in the order the walk wrote them, with a
//! `(start, end)` per point: reverse postorder is not point order, so a point's items are found
//! by its range, not by a sort. The unreachable blocks are read after the reachable ones for
//! what is asked of every block (promotion, the locals a constant or a static is read into),
//! which their statements change as the reachable ones' do.

use hir_def::hir::Expr;
use la_arena::RawIdx;

use hir_ty::mir::{
    AggregateKind, AsmOperand, BasicBlock, BasicBlockId, BorrowKind, LocalId, MirBody, MirSpan,
    Operand, OperandKind, PlaceElem, ProjectionElem, Rvalue, Statement, StatementKind,
    StoredPlace, Terminator, TerminatorKind,
};

use crate::{
    Ctx,
    cfg::{Cfg, Location, index},
    mem::{FVec, Frame},
    places::PlaceId,
};

pub(crate) const NONE: u32 = u32::MAX;

pub(crate) fn raw(local: LocalId) -> usize {
    local.into_raw().into_u32() as usize
}

pub(crate) fn local(raw: u32) -> LocalId {
    LocalId::from_raw(RawIdx::from_u32(raw))
}

/// Items by point: point `p`'s are `items[at[p].0..at[p].1]`.
pub(crate) struct Column<'f, T> {
    at: &'f [(u32, u32)],
    items: &'f [T],
}

impl<T> Clone for Column<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Column<'_, T> {}

impl<'f, T> Column<'f, T> {
    #[inline]
    pub(crate) fn of(&self, point: usize) -> &'f [T] {
        let (start, end) = self.at[point];
        &self.items[start as usize..end as usize]
    }
}

/// A [`Column`] being written a point at a time.
struct Building<'f, T> {
    at: &'f mut [(u32, u32)],
    items: FVec<'f, T>,
    open: u32,
}

impl<'f, T> Building<'f, T> {
    fn new(frame: &'f Frame, points: usize, room: usize) -> Self {
        Building {
            at: frame.alloc_slice_fill_with(points, |_| (0, 0)),
            items: Vec::with_capacity_in(room, frame),
            open: 0,
        }
    }

    #[inline]
    fn push(&mut self, item: T) {
        self.items.push(item);
    }

    #[inline]
    fn begin(&mut self) {
        self.open = self.items.len() as u32;
    }

    #[inline]
    fn end(&mut self, point: usize) {
        self.at[point] = (self.open, self.items.len() as u32);
    }

    fn freeze(self) -> Column<'f, T> {
        Column { at: self.at, items: self.items.leak() }
    }
}

/// What decides whether a place an operation names is moved out of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MoveKind {
    /// A `move` operand: moved if its type is not `Copy` and it is not a variable its arm's
    /// guard is being evaluated for. Not in a comparison, which takes its operands by
    /// reference.
    Operand,
    /// The discriminant read of an `as` cast of an enum: moved if its type is not `Copy`.
    Cast,
    /// `Deinit` and `Drop`.
    Always,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Moved {
    pub(crate) place: PlaceId,
    /// The operand's span, or the statement's or terminator's where lowering kept none.
    pub(crate) span: MirSpan,
    pub(crate) kind: MoveKind,
}

/// What writes a place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum WriteKind {
    Assign,
    /// An `asm!` output.
    Asm,
    DropAndReplace,
    /// A call's destination, written only if it returns.
    Call,
    /// A `yield`'s resume argument.
    Yield,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Written {
    pub(crate) place: PlaceId,
    pub(crate) kind: WriteKind,
}

/// An `Assign(assigned, Ref(kind, borrowed))`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Borrow {
    pub(crate) location: Location,
    pub(crate) kind: BorrowKind,
    pub(crate) span: MirSpan,
    pub(crate) assigned: LocalId,
    /// The reference is assigned to the whole of `assigned`.
    pub(crate) assigned_whole: bool,
    pub(crate) local: LocalId,
    /// The place borrowed; [`NONE`] in an unreachable block, whose places are not typed.
    pub(crate) place: PlaceId,
    /// The place borrowed is the whole of `local`.
    pub(crate) whole: bool,
    pub(crate) through_deref: bool,
}

/// An `Assign(local, Use(operand))` of a constant or a static, `local` whole.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Constant {
    pub(crate) local: LocalId,
    pub(crate) is_static: bool,
    /// A `static mut`'s.
    pub(crate) mutable: bool,
}

/// How a local is defined, for promotion (rustc's `TempState`): what a temporary defined once
/// from constants and other temporaries is made of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Temp {
    Undefined,
    /// Defined once, by a use or an aggregate (not a union's) of constants and of the whole of
    /// the locals of `temp_locals[start..end]`.
    Of(u32, u32),
    /// Defined more than once, in part, or by anything else.
    Not,
}

/// A place decoded: its local and its projection's range of `elems`.
#[derive(Clone, Copy)]
struct Interned {
    local: u32,
    start: u32,
    end: u32,
}

const UNSET: Interned = Interned { local: NONE, start: 0, end: 0 };

/// The columns of one body.
#[derive(Clone, Copy)]
pub(crate) struct Decoded<'f> {
    /// The places moved out of at each point, in the order the operations name them.
    pub(crate) moves: Column<'f, Moved>,
    /// The places written at each point, in order, after its moves.
    pub(crate) writes: Column<'f, Written>,
    /// The locals each point uses, for liveness (`Drop`'s of a whole local aside).
    pub(crate) uses: Column<'f, u32>,
    /// The locals each point defines: assigns whole, or begins or ends the storage of.
    pub(crate) defs: Column<'f, u32>,
    /// The locals each point reads through an operand or a place (a two-phase borrow is
    /// activated by the first).
    pub(crate) two_phase: Column<'f, u32>,
    /// The local each point ends the storage of, [`NONE`] for none.
    pub(crate) dead: &'f [u32],
    /// By block: the block its call returns to, [`NONE`] for none.
    pub(crate) returns: &'f [u32],
    /// By block: it has a terminator.
    pub(crate) terminated: &'f [bool],
    /// By block: the local its `Drop` drops whole, [`NONE`] for none.
    pub(crate) dropped: &'f [u32],
    /// By block: the block it certainly goes on to, through a `Goto`, a returning call, a
    /// `Drop` or an `Assert`; [`NONE`] for none.
    pub(crate) certain: &'f [u32],
    /// Every block's, the reachable ones' first in reverse postorder and statement order.
    borrows: &'f [Borrow],
    reachable_borrows: usize,
    /// Every block's.
    pub(crate) constants: &'f [Constant],
    temps: &'f [Temp],
    temp_locals: &'f [LocalId],
    roots: usize,
    places: &'f [Interned],
    elems: &'f [PlaceElem],
}

impl<'f> Decoded<'f> {
    pub(crate) fn new<'db>(ctx: &Ctx<'f, 'db>, body: &MirBody<'db>, cfg: &Cfg<'_>) -> Self {
        let frame = ctx.frame;
        let points = cfg.points;
        let blocks = body.basic_blocks.len();
        let roots = body.locals.len();
        let mut decoder = Decoder {
            ctx,
            body,
            roots,
            places: Vec::with_capacity_in(roots, frame),
            elems: Vec::with_capacity_in(roots, frame),
            moves: Building::new(frame, points, points / 2),
            writes: Building::new(frame, points, points / 2),
            uses: Building::new(frame, points, points * 2),
            defs: Building::new(frame, points, points),
            two_phase: Building::new(frame, points, points),
            dead: frame.alloc_slice_fill_with(points, |_| NONE),
            returns: frame.alloc_slice_fill_with(blocks, |_| NONE),
            terminated: frame.alloc_slice_fill_with(blocks, |_| false),
            dropped: frame.alloc_slice_fill_with(blocks, |_| NONE),
            certain: frame.alloc_slice_fill_with(blocks, |_| NONE),
            borrows: Vec::with_capacity_in(points / 8, frame),
            constants: Vec::new_in(frame),
            temps: frame.alloc_slice_fill_with(roots, |_| Temp::Undefined),
            temp_locals: Vec::new_in(frame),
        };
        for &block in cfg.order.iter() {
            decoder.reached(cfg, block, &body.basic_blocks[block]);
        }
        let reachable_borrows = decoder.borrows.len();
        for (block, data) in body.basic_blocks.iter() {
            if !cfg.is_reachable(block) {
                decoder.unreached(block, data);
            }
        }
        decoder.freeze(reachable_borrows)
    }

    /// The borrows of the reachable blocks, in reverse postorder and statement order.
    pub(crate) fn reachable_borrows(&self) -> &'f [Borrow] {
        &self.borrows[..self.reachable_borrows]
    }

    /// The borrows of every block.
    pub(crate) fn borrows(&self) -> &'f [Borrow] {
        self.borrows
    }

    /// What `local` is defined from once, if it is defined once from constants and the whole
    /// of the locals given, by a use or an aggregate that is not a union's.
    pub(crate) fn temp_of(&self, local: LocalId) -> Option<&'f [LocalId]> {
        match self.temps[raw(local)] {
            Temp::Of(start, end) => Some(&self.temp_locals[start as usize..end as usize]),
            Temp::Undefined | Temp::Not => None,
        }
    }

    /// The local of a place the walk decoded.
    pub(crate) fn local(&self, place: PlaceId) -> LocalId {
        match (place as usize).checked_sub(self.roots) {
            None => local(place),
            Some(at) => local(self.places[at].local),
        }
    }

    /// The projection of a place the walk decoded.
    pub(crate) fn projection(&self, place: PlaceId) -> &'f [PlaceElem] {
        match (place as usize).checked_sub(self.roots) {
            None => &[],
            Some(at) => {
                let it = self.places[at];
                &self.elems[it.start as usize..it.end as usize]
            }
        }
    }
}

struct Decoder<'c, 'f, 'db> {
    ctx: &'c Ctx<'f, 'db>,
    body: &'c MirBody<'db>,
    roots: usize,
    places: FVec<'f, Interned>,
    elems: FVec<'f, PlaceElem>,
    moves: Building<'f, Moved>,
    writes: Building<'f, Written>,
    uses: Building<'f, u32>,
    defs: Building<'f, u32>,
    two_phase: Building<'f, u32>,
    dead: &'f mut [u32],
    returns: &'f mut [u32],
    terminated: &'f mut [bool],
    dropped: &'f mut [u32],
    certain: &'f mut [u32],
    borrows: FVec<'f, Borrow>,
    constants: FVec<'f, Constant>,
    temps: &'f mut [Temp],
    temp_locals: FVec<'f, LocalId>,
}

impl<'f> Decoder<'_, 'f, '_> {
    fn reached(&mut self, cfg: &Cfg<'_>, block: BasicBlockId, data: &BasicBlock) {
        for (statement, it) in data.statements.iter().enumerate() {
            let location = Location { block, statement };
            let point = cfg.point(location);
            self.begin();
            self.statement(location, point, it);
            self.all_blocks(location, it, true);
            self.end(point);
        }
        let location = Location { block, statement: data.statements.len() };
        let point = cfg.point(location);
        self.begin();
        if let Some(terminator) = &data.terminator {
            self.terminated[index(block)] = true;
            self.terminator(block, terminator);
        }
        self.end(point);
    }

    /// The facts asked of every block, of an unreachable one.
    fn unreached(&mut self, block: BasicBlockId, data: &BasicBlock) {
        for (statement, it) in data.statements.iter().enumerate() {
            self.all_blocks(Location { block, statement }, it, false);
        }
        if let Some(terminator) = &data.terminator
            && let TerminatorKind::Call { destination, .. } = &terminator.kind
        {
            self.define(destination.local, None);
        }
    }

    fn begin(&mut self) {
        self.moves.begin();
        self.writes.begin();
        self.uses.begin();
        self.defs.begin();
        self.two_phase.begin();
    }

    fn end(&mut self, point: usize) {
        self.moves.end(point);
        self.writes.end(point);
        self.uses.end(point);
        self.defs.end(point);
        self.two_phase.end(point);
    }

    fn intern(&mut self, place: &StoredPlace) -> PlaceId {
        let projection = place.projection.as_slice();
        let id = self.ctx.place(place.local, projection);
        if let Some(at) = (id as usize).checked_sub(self.roots) {
            if at >= self.places.len() {
                self.places.resize(at + 1, UNSET);
            }
            if self.places[at].local == NONE {
                let start = self.elems.len() as u32;
                self.elems.extend_from_slice(projection);
                self.places[at] =
                    Interned { local: raw(place.local) as u32, start, end: self.elems.len() as u32 };
            }
        }
        id
    }

    fn moved(&mut self, operand: &Operand, span: MirSpan) {
        if let OperandKind::Move(place) = &operand.kind {
            let place = self.intern(place);
            let span = operand.span().unwrap_or(span);
            self.moves.push(Moved { place, span, kind: MoveKind::Operand });
        }
    }

    fn write(&mut self, place: &StoredPlace, kind: WriteKind) {
        let place = self.intern(place);
        self.writes.push(Written { place, kind });
    }

    fn place_uses(&mut self, local: LocalId, projection: &[PlaceElem]) {
        self.uses.push(raw(local) as u32);
        for elem in projection {
            if let PlaceElem::Index(index) = elem {
                self.uses.push(raw(*index) as u32);
            }
        }
    }

    fn operand_uses(&mut self, operand: &Operand) {
        if let OperandKind::Copy(place) | OperandKind::Move(place) = &operand.kind {
            self.place_uses(place.local, place.projection.as_slice());
        }
    }

    /// A write of a whole local defines it; a write of part of it uses it.
    fn assigned(&mut self, place: &StoredPlace) {
        let projection = place.projection.as_slice();
        if projection.is_empty() {
            self.defs.push(raw(place.local) as u32);
        } else {
            self.place_uses(place.local, projection);
        }
    }

    fn operand_read(&mut self, operand: &Operand) {
        if let OperandKind::Copy(place) | OperandKind::Move(place) = &operand.kind {
            self.two_phase.push(raw(place.local) as u32);
        }
    }

    fn statement(&mut self, location: Location, point: usize, it: &Statement) {
        let span = it.span;
        let compares =
            matches!(&it.kind, StatementKind::Assign(_, rvalue) if crate::compares(rvalue));
        if !compares {
            it.kind.for_each_operand(&mut |operand| self.moved(operand, span));
        }
        match &it.kind {
            StatementKind::Assign(place, rvalue) => {
                if let Rvalue::Discriminant(moved) = rvalue
                    && let MirSpan::ExprId(expr) = span
                    && matches!(self.ctx.store.expr(expr), Expr::Cast { .. })
                {
                    let moved = self.intern(moved);
                    self.moves.push(Moved { place: moved, span, kind: MoveKind::Cast });
                }
                self.write(place, WriteKind::Assign);
                match rvalue {
                    Rvalue::Ref(_, read)
                    | Rvalue::Len(read)
                    | Rvalue::Discriminant(read)
                    | Rvalue::CopyForDeref(read) => {
                        self.place_uses(read.local, read.projection.as_slice());
                    }
                    _ => rvalue.for_each_operand(&mut |operand| self.operand_uses(operand)),
                }
                self.assigned(place);
                rvalue.for_each_operand(&mut |operand| self.operand_read(operand));
                if let Rvalue::Ref(_, read)
                | Rvalue::CopyForDeref(read)
                | Rvalue::Len(read)
                | Rvalue::Discriminant(read) = rvalue
                {
                    self.two_phase.push(raw(read.local) as u32);
                }
                if !place.projection.is_empty() {
                    self.two_phase.push(raw(place.local) as u32);
                }
                if let Rvalue::Ref(kind, borrowed) = rvalue {
                    let projection = borrowed.projection.as_slice();
                    let place_id = self.intern(borrowed);
                    self.borrows.push(Borrow {
                        location,
                        kind: *kind,
                        span,
                        assigned: place.local,
                        assigned_whole: place.projection.is_empty(),
                        local: borrowed.local,
                        place: place_id,
                        whole: projection.is_empty(),
                        through_deref: projection.contains(&ProjectionElem::Deref),
                    });
                }
            }
            StatementKind::FakeRead(place) => {
                if !crate::liveness::reads_nothing(self.ctx, self.body, place, span) {
                    self.place_uses(place.local, place.projection.as_slice());
                }
            }
            StatementKind::Deinit(place) => {
                let moved = self.intern(place);
                self.moves.push(Moved { place: moved, span, kind: MoveKind::Always });
                self.assigned(place);
            }
            StatementKind::StorageLive(local) => self.defs.push(raw(*local) as u32),
            StatementKind::StorageDead(local) => {
                self.dead[point] = raw(*local) as u32;
                self.defs.push(raw(*local) as u32);
            }
            StatementKind::InlineAsm(asm) => {
                for operand in &asm.operands {
                    if let AsmOperand::Reg { output: Some(output), .. } = operand {
                        self.write(output, WriteKind::Asm);
                    }
                }
                it.kind.for_each_operand(&mut |operand| self.operand_uses(operand));
            }
            StatementKind::Nop => (),
        }
    }

    fn terminator(&mut self, block: BasicBlockId, terminator: &Terminator) {
        let at = index(block);
        let span = terminator.span;
        terminator.kind.for_each_operand(&mut |operand| self.moved(operand, span));
        terminator.kind.for_each_operand(&mut |operand| self.operand_read(operand));
        match &terminator.kind {
            TerminatorKind::Drop { place, target, .. } => {
                let moved = self.intern(place);
                self.moves.push(Moved { place: moved, span, kind: MoveKind::Always });
                let projection = place.projection.as_slice();
                if projection.is_empty() {
                    self.dropped[at] = raw(place.local) as u32;
                } else {
                    self.place_uses(place.local, projection);
                }
                self.certain[at] = index(*target) as u32;
            }
            TerminatorKind::DropAndReplace { place, value, .. } => {
                self.write(place, WriteKind::DropAndReplace);
                self.operand_uses(value);
                self.place_uses(place.local, place.projection.as_slice());
            }
            TerminatorKind::Call { func, args, destination, target, .. } => {
                self.write(destination, WriteKind::Call);
                self.assigned(destination);
                self.operand_uses(func);
                for arg in args.iter() {
                    self.operand_uses(arg);
                }
                self.define(destination.local, None);
                if let Some(target) = target {
                    self.returns[at] = index(*target) as u32;
                    self.certain[at] = index(*target) as u32;
                }
            }
            TerminatorKind::Yield { resume_arg, .. } => {
                self.write(resume_arg, WriteKind::Yield);
                terminator.kind.for_each_operand(&mut |operand| self.operand_uses(operand));
            }
            TerminatorKind::Return => self.uses.push(raw(crate::return_slot()) as u32),
            kind => {
                if let TerminatorKind::Goto { target } | TerminatorKind::Assert { target, .. } =
                    kind
                {
                    self.certain[at] = index(*target) as u32;
                }
                kind.for_each_operand(&mut |operand| self.operand_uses(operand));
            }
        }
    }

    /// What every block's statement says: the locals constants are read into, the borrows of
    /// an unreachable block (a reachable one's are made by [`Self::statement`]), and how each
    /// local is defined.
    fn all_blocks(&mut self, location: Location, it: &Statement, reached: bool) {
        match &it.kind {
            StatementKind::Assign(place, rvalue) => {
                if let Rvalue::Use(operand) = rvalue
                    && !matches!(operand.kind, OperandKind::Copy(_) | OperandKind::Move(_))
                    && place.projection.is_empty()
                {
                    let is_static = matches!(operand.kind, OperandKind::Static(_));
                    let mutable = match operand.kind {
                        OperandKind::Static(id) => {
                            hir_def::signatures::StaticSignature::of(self.ctx.db, id)
                                .flags
                                .contains(hir_def::signatures::StaticFlags::MUTABLE)
                        }
                        _ => false,
                    };
                    self.constants.push(Constant { local: place.local, is_static, mutable });
                }
                if let Rvalue::Ref(kind, borrowed) = rvalue
                    && !reached
                {
                    let projection = borrowed.projection.as_slice();
                    self.borrows.push(Borrow {
                        location,
                        kind: *kind,
                        span: it.span,
                        assigned: place.local,
                        assigned_whole: place.projection.is_empty(),
                        local: borrowed.local,
                        place: NONE,
                        whole: projection.is_empty(),
                        through_deref: projection.contains(&ProjectionElem::Deref),
                    });
                }
                let whole = place.projection.is_empty();
                self.define(place.local, whole.then_some(rvalue));
                // A mutable borrow of a part of a temporary is a use rustc's `TempState` takes
                // for a mutation, `&mut t.0` of `let ref a @ (ref mut b, _) = (1, 2)`.
                if let Rvalue::Ref(BorrowKind::Mut { .. }, borrowed) = rvalue
                    && !borrowed.projection.as_slice().is_empty()
                {
                    self.define(borrowed.local, None);
                }
            }
            StatementKind::Deinit(place) => self.define(place.local, None),
            _ => (),
        }
    }

    /// rustc's `TempState` of `local` after one more definition, `rvalue` when it assigns the
    /// whole local.
    fn define(&mut self, local: LocalId, rvalue: Option<&Rvalue>) {
        let at = raw(local);
        let next = match (self.temps[at], rvalue) {
            (Temp::Undefined, Some(rvalue)) => self.shape(rvalue),
            _ => Temp::Not,
        };
        self.temps[at] = next;
    }

    fn shape(&mut self, rvalue: &Rvalue) -> Temp {
        let start = self.temp_locals.len();
        let operand = |locals: &mut FVec<'f, LocalId>, operand: &Operand| match &operand.kind {
            OperandKind::Constant { .. } | OperandKind::Allocation { .. } => true,
            OperandKind::Copy(place) | OperandKind::Move(place) => {
                let whole = place.projection.is_empty();
                if whole {
                    locals.push(place.local);
                }
                whole
            }
            OperandKind::Static(_) => false,
        };
        let ok = match rvalue {
            Rvalue::Use(it) => operand(&mut self.temp_locals, it),
            // A closure that captures nothing too, `unwrap_or(&|_| true)`.
            Rvalue::Aggregate(kind, operands) => {
                !matches!(kind, AggregateKind::Union(..))
                    && operands.iter().all(|it| operand(&mut self.temp_locals, it))
            }
            _ => false,
        };
        if ok {
            Temp::Of(start as u32, self.temp_locals.len() as u32)
        } else {
            self.temp_locals.truncate(start);
            Temp::Not
        }
    }

    fn freeze(self, reachable_borrows: usize) -> Decoded<'f> {
        Decoded {
            moves: self.moves.freeze(),
            writes: self.writes.freeze(),
            uses: self.uses.freeze(),
            defs: self.defs.freeze(),
            two_phase: self.two_phase.freeze(),
            dead: self.dead,
            returns: self.returns,
            terminated: self.terminated,
            dropped: self.dropped,
            certain: self.certain,
            borrows: self.borrows.leak(),
            reachable_borrows,
            constants: self.constants.leak(),
            temps: self.temps,
            temp_locals: self.temp_locals.leak(),
            roots: self.roots,
            places: self.places.leak(),
            elems: self.elems.leak(),
        }
    }
}

#[cfg(test)]
mod tests {
    use blaze_ids::rng::SplitMix64;
    use blaze_mem::{Tag, frame};

    use super::{Building, Column};
    use crate::mem::Frame;

    fn column_of<'f>(frame: &'f Frame, points: &[&[u32]], order: &[usize]) -> Column<'f, u32> {
        let mut building = Building::new(frame, points.len(), 0);
        for &point in order {
            building.begin();
            for &it in points[point] {
                building.push(it);
            }
            building.end(point);
        }
        building.freeze()
    }

    /// A column written in any order of its points gives each point what was written for it.
    #[test]
    fn a_column_gives_each_point_its_items() {
        frame::task(Tag::Mir, |frame| {
            let mut rng = SplitMix64::new(0x9e37_79b9_7f4a_7c15);
            for _ in 0..300 {
                let count = 1 + rng.index(40);
                let points: Vec<Vec<u32>> = (0..count)
                    .map(|_| (0..rng.index(4)).map(|_| rng.index(1000) as u32).collect())
                    .collect();
                let slices: Vec<&[u32]> = points.iter().map(|it| it.as_slice()).collect();
                let mut order: Vec<usize> = (0..count).collect();
                rng.shuffle(&mut order);
                // Some points are never written: the unreachable blocks'.
                order.truncate(1 + rng.index(count));
                let column = column_of(frame, &slices, &order);
                for point in 0..count {
                    let want: &[u32] = if order.contains(&point) { &points[point] } else { &[] };
                    assert_eq!(column.of(point), want);
                }
            }
        });
    }
}
